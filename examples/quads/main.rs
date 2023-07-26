use bevy::{
    core_pipeline::core_3d,
    diagnostic::{FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin},
    ecs::{
        query::{QueryItem, ROQueryItem},
        system::{
            lifetimeless::{Read, SRes},
            SystemParamItem,
        },
    },
    prelude::*,
    reflect::TypeUuid,
    render::{
        camera::ExtractedCamera,
        extract_resource::{ExtractResource, ExtractResourcePlugin},
        mesh::PrimitiveTopology,
        render_graph::{
            NodeRunError, RenderGraphApp, RenderGraphContext, ViewNode, ViewNodeRunner,
        },
        render_phase::{
            AddRenderCommand, CachedRenderPipelinePhaseItem, DrawFunctionId, DrawFunctions,
            PhaseItem, RenderCommand, RenderCommandResult, RenderPhase, SetItemPipeline,
            TrackedRenderPass,
        },
        render_resource::{
            BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
            BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, Buffer,
            BufferBindingType, BufferInitDescriptor, BufferSize, BufferUsages,
            CachedRenderPipelineId, ColorTargetState, ColorWrites, CompareFunction, DepthBiasState,
            DepthStencilState, Face, FragmentState, FrontFace, IndexFormat, LoadOp,
            MultisampleState, Operations, PipelineCache, PolygonMode, PrimitiveState,
            RenderPassDepthStencilAttachment, RenderPassDescriptor, RenderPipelineDescriptor,
            ShaderStages, ShaderType, StencilFaceState, StencilState, StorageBuffer, TextureFormat,
            VertexState,
        },
        renderer::{RenderContext, RenderDevice, RenderQueue},
        texture::BevyDefault,
        view::{ViewDepthTexture, ViewTarget, ViewUniform, ViewUniformOffset, ViewUniforms},
        Extract, Render, RenderApp, RenderSet,
    },
};
use bytemuck::cast_slice;
use examples_utils::camera::{CameraController, CameraControllerPlugin};
use rand::Rng;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: format!(
                    "{} {} - quads",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                ),
                resolution: (1920.0, 1080.0).into(),
                ..Default::default()
            }),
            ..default()
        }))
        .add_plugins((
            CameraControllerPlugin,
            FrameTimeDiagnosticsPlugin,
            LogDiagnosticsPlugin::default(),
            QuadsPlugin,
        ))
        .add_systems(Startup, setup)
        .run();
}

#[derive(Clone, Debug, Default)]
pub enum Billboard {
    #[default]
    None,
    ViewY,
    WorldY,
    FixedScreenSize,
}

#[derive(Clone, Debug, Default)]
struct Quad {
    color: Color,
    center: Vec3,
    /// Half-extents are in world units except for in Billboard::FixedScreenSize mode, then they are
    /// in screen pixels
    half_extents: Vec3,
    billboard: Billboard,
}

impl Quad {
    pub fn random<R: Rng + ?Sized>(
        rng: &mut R,
        min: Vec3,
        max: Vec3,
        half_extents: Vec3,
        billboard: Billboard,
    ) -> Self {
        Self {
            color: Color::WHITE,
            center: random_point_vec3(rng, min, max),
            half_extents,
            billboard,
        }
    }
}

fn random_point_vec3<R: Rng + ?Sized>(rng: &mut R, min: Vec3, max: Vec3) -> Vec3 {
    Vec3::new(
        rng.gen_range(min.x..max.x),
        rng.gen_range(min.y..max.y),
        rng.gen_range(min.z..max.z),
    )
}

#[derive(Clone, Debug, Default, Resource, ExtractResource)]
struct Quads {
    data: Vec<Quad>,
}

fn setup(mut commands: Commands) {
    commands
        .spawn(Camera3dBundle {
            transform: Transform::from_translation(50.0 * Vec3::Z).looking_at(Vec3::ZERO, Vec3::Y),
            ..default()
        })
        .insert(CameraController::default());

    let mut quads = Quads::default();
    let mut rng = rand::thread_rng();
    let min = -10.0 * Vec3::ONE;
    let max = 10.0 * Vec3::ONE;
    let n_quads = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<usize>().ok())
        .unwrap_or(1_000_000);
    info!("Generating {} quads", n_quads);
    for _ in 0..n_quads {
        quads.data.push(Quad::random(
            &mut rng,
            min,
            max,
            0.01 * Vec3::ONE,
            Billboard::ViewY,
        ));
    }
    commands.insert_resource(quads);
}

fn extract_quads_phase(mut commands: Commands, cameras: Extract<Query<Entity, With<Camera3d>>>) {
    for entity in cameras.iter() {
        commands
            .get_or_spawn(entity)
            .insert(RenderPhase::<QuadsPhaseItem>::default());
    }
}

// NOTE: These must match the bit flags in quads.wgsl!
bitflags::bitflags! {
    #[repr(transparent)]
    pub struct GpuQuadFlags: u32 {
        const BILLBOARD                   = (1 << 0);
        const BILLBOARD_WORLD_Y           = (1 << 1);
        const BILLBOARD_FIXED_SCREEN_SIZE = (1 << 2);
    }
}

#[derive(Clone, Copy, Debug, Default, ShaderType)]
struct GpuQuad {
    center: Vec3,
    flags: u32,
    half_extents: Vec4,
    color: [f32; 4],
}

impl From<&Quad> for GpuQuad {
    fn from(quad: &Quad) -> Self {
        Self {
            center: quad.center,
            flags: match quad.billboard {
                Billboard::None => GpuQuadFlags::empty(),
                Billboard::ViewY => GpuQuadFlags::BILLBOARD,
                Billboard::WorldY => GpuQuadFlags::BILLBOARD | GpuQuadFlags::BILLBOARD_WORLD_Y,
                Billboard::FixedScreenSize => GpuQuadFlags::BILLBOARD_FIXED_SCREEN_SIZE,
            }
            .bits(),
            half_extents: quad.half_extents.extend(0.0),
            color: quad.color.as_rgba_f32(),
        }
    }
}

#[derive(Resource)]
struct GpuQuads {
    index_buffer: Option<Buffer>,
    index_count: u32,
    instances: StorageBuffer<GpuQuadsArray>,
    bind_group: Option<BindGroup>,
}

#[derive(Default, ShaderType)]
struct GpuQuadsArray {
    #[size(runtime)]
    array: Vec<GpuQuad>,
}

impl Default for GpuQuads {
    fn default() -> Self {
        let mut instances = StorageBuffer::<GpuQuadsArray>::default();
        instances.set_label(Some("gpu_quads_array"));
        Self {
            index_buffer: None,
            index_count: 0,
            instances,
            bind_group: None,
        }
    }
}

#[derive(Component)]
struct GpuQuadsMarker;

fn prepare_quads(
    mut commands: Commands,
    quads: Option<Res<Quads>>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    gpu_quads: Option<ResMut<GpuQuads>>,
) {
    if let Some(quads) = quads {
        if quads.is_changed() {
            let mut new_gpu_quads = None;
            let gpu_quads = if let Some(gpu_quads) = gpu_quads {
                gpu_quads.into_inner()
            } else {
                new_gpu_quads = Some(GpuQuads::default());
                new_gpu_quads.as_mut().unwrap()
            };
            for quad in quads.data.iter() {
                gpu_quads
                    .instances
                    .get_mut()
                    .array
                    .push(GpuQuad::from(quad));
            }
            let n_instances = gpu_quads.instances.get().array.len();
            gpu_quads.index_count = n_instances as u32 * 6;
            let mut indices = Vec::with_capacity(gpu_quads.index_count as usize);
            for i in 0..n_instances {
                let base = (i * 4) as u32;
                indices.push(base + 2);
                indices.push(base);
                indices.push(base + 1);
                indices.push(base + 1);
                indices.push(base + 3);
                indices.push(base + 2);
            }
            gpu_quads.index_buffer = Some(render_device.create_buffer_with_data(
                &BufferInitDescriptor {
                    label: Some("gpu_quads_index_buffer"),
                    contents: cast_slice(&indices),
                    usage: BufferUsages::INDEX,
                },
            ));

            gpu_quads
                .instances
                .write_buffer(&*render_device, &*render_queue);

            if let Some(new_gpu_quads) = new_gpu_quads {
                commands.insert_resource(new_gpu_quads);
            }
        }
        commands.spawn(GpuQuadsMarker);
    }
}

pub struct QuadsPhaseItem {
    pub draw_function: DrawFunctionId,
    pub entity: Entity,
    pub pipeline: CachedRenderPipelineId,
}

impl PhaseItem for QuadsPhaseItem {
    type SortKey = u32;

    #[inline]
    fn sort_key(&self) -> Self::SortKey {
        0
    }

    #[inline]
    fn draw_function(&self) -> DrawFunctionId {
        self.draw_function
    }

    fn entity(&self) -> Entity {
        self.entity
    }
}

impl CachedRenderPipelinePhaseItem for QuadsPhaseItem {
    fn cached_pipeline(&self) -> CachedRenderPipelineId {
        self.pipeline
    }
}

#[derive(Resource)]
pub struct GpuQuadsViewBindGroup {
    bind_group: BindGroup,
}

fn queue_quads(
    mut commands: Commands,
    opaque_3d_draw_functions: Res<DrawFunctions<QuadsPhaseItem>>,
    quads_pipeline: Res<QuadsPipeline>,
    render_device: Res<RenderDevice>,
    view_uniforms: Res<ViewUniforms>,
    mut gpu_quads: Option<ResMut<GpuQuads>>,
    entities: Query<Entity, With<GpuQuadsMarker>>,
    mut views: Query<&mut RenderPhase<QuadsPhaseItem>>,
) {
    let draw_quads = opaque_3d_draw_functions
        .read()
        .get_id::<DrawQuads>()
        .unwrap();

    commands.insert_resource(GpuQuadsViewBindGroup {
        bind_group: render_device.create_bind_group(&BindGroupDescriptor {
            label: Some("gpu_quads_view_bind_group"),
            layout: &quads_pipeline.view_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: view_uniforms.uniforms.binding().unwrap(),
            }],
        }),
    });

    if let Some(gpu_quads) = gpu_quads.as_mut() {
        if gpu_quads.is_changed() {
            println!("GpuQuads changed");
            gpu_quads.bind_group = Some(render_device.create_bind_group(&BindGroupDescriptor {
                label: Some("gpu_quads_bind_group"),
                layout: &quads_pipeline.quads_layout,
                entries: &[BindGroupEntry {
                    binding: 0,
                    resource: gpu_quads.instances.buffer().unwrap().as_entire_binding(),
                }],
            }));
        }
    }

    for entity in &entities {
        for mut opaque_phase in views.iter_mut() {
            opaque_phase.add(QuadsPhaseItem {
                entity,
                draw_function: draw_quads,
                pipeline: quads_pipeline.pipeline_id,
            });
        }
    }
}

mod node {
    pub const QUADS_PASS: &str = "quads_pass";
}

#[derive(Default)]
pub struct QuadsPassNode;

impl ViewNode for QuadsPassNode {
    type ViewQuery = (
        &'static ExtractedCamera,
        &'static RenderPhase<QuadsPhaseItem>,
        &'static ViewTarget,
        &'static ViewDepthTexture,
    );
    fn run(
        &self,
        graph: &mut RenderGraphContext,
        render_context: &mut RenderContext,
        (camera, quads_phase, target, depth): QueryItem<Self::ViewQuery>,
        world: &World,
    ) -> Result<(), NodeRunError> {
        let view_entity = graph.view_entity();

        #[cfg(feature = "trace")]
        let _main_quads_pass_span = info_span!("main_quads_pass").entered();
        let pass_descriptor = RenderPassDescriptor {
            label: Some("main_quads_pass"),
            // NOTE: The quads pass loads the color
            // buffer as well as writing to it.
            color_attachments: &[Some(target.get_color_attachment(Operations {
                load: LoadOp::Load,
                store: true,
            }))],
            depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                view: &depth.view,
                // NOTE: The quads main pass loads the depth buffer and possibly overwrites it
                depth_ops: Some(Operations {
                    load: LoadOp::Load,
                    store: true,
                }),
                stencil_ops: None,
            }),
        };

        let mut render_pass = render_context.begin_tracked_render_pass(pass_descriptor);

        if let Some(viewport) = camera.viewport.as_ref() {
            render_pass.set_camera_viewport(viewport);
        }

        quads_phase.render(&mut render_pass, world, view_entity);

        Ok(())
    }
}

struct QuadsPlugin;

impl Plugin for QuadsPlugin {
    fn build(&self, app: &mut App) {
        app.world.resource_mut::<Assets<Shader>>().set_untracked(
            QUADS_SHADER_HANDLE,
            Shader::from_wgsl(include_str!("quads.wgsl"), "quads.wgsl"),
        );
        app.add_plugins(ExtractResourcePlugin::<Quads>::default());

        let render_app = app.sub_app_mut(RenderApp);

        render_app
            .init_resource::<DrawFunctions<QuadsPhaseItem>>()
            .add_render_command::<QuadsPhaseItem, DrawQuads>()
            .add_render_graph_node::<ViewNodeRunner<QuadsPassNode>>(
                core_3d::graph::NAME,
                node::QUADS_PASS,
            )
            .add_render_graph_edge(
                core_3d::graph::NAME,
                core_3d::graph::node::END_MAIN_PASS,
                node::QUADS_PASS,
            )
            .add_systems(ExtractSchedule, extract_quads_phase)
            .add_systems(
                Render,
                (
                    prepare_quads.in_set(RenderSet::Prepare),
                    queue_quads.in_set(RenderSet::Queue),
                ),
            );
    }
    fn finish(&self, app: &mut App) {
        let render_app = app.sub_app_mut(RenderApp);
        render_app.init_resource::<QuadsPipeline>();
    }
}

#[derive(Resource)]
struct QuadsPipeline {
    pipeline_id: CachedRenderPipelineId,
    view_layout: BindGroupLayout,
    quads_layout: BindGroupLayout,
}

const QUADS_SHADER_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 7659167879172469997);

impl FromWorld for QuadsPipeline {
    fn from_world(world: &mut World) -> Self {
        let view_layout =
            world
                .resource::<RenderDevice>()
                .create_bind_group_layout(&BindGroupLayoutDescriptor {
                    entries: &[
                        // View
                        BindGroupLayoutEntry {
                            binding: 0,
                            visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                            ty: BindingType::Buffer {
                                ty: BufferBindingType::Uniform,
                                has_dynamic_offset: true,
                                min_binding_size: Some(ViewUniform::min_size()),
                            },
                            count: None,
                        },
                    ],
                    label: Some("shadow_view_layout"),
                });

        let quads_layout =
            world
                .resource::<RenderDevice>()
                .create_bind_group_layout(&BindGroupLayoutDescriptor {
                    label: None,
                    entries: &[BindGroupLayoutEntry {
                        binding: 0,
                        visibility: ShaderStages::VERTEX,
                        ty: BindingType::Buffer {
                            ty: BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: BufferSize::new(0),
                        },
                        count: None,
                    }],
                });

        let pipeline_cache = world.resource_mut::<PipelineCache>();
        let pipeline_id = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("quads_pipeline".into()),
            layout: vec![view_layout.clone(), quads_layout.clone()],
            vertex: VertexState {
                shader: QUADS_SHADER_HANDLE.typed(),
                shader_defs: vec![],
                entry_point: "vertex".into(),
                buffers: vec![],
            },
            fragment: Some(FragmentState {
                shader: QUADS_SHADER_HANDLE.typed(),
                shader_defs: vec![],
                entry_point: "fragment".into(),
                targets: vec![Some(ColorTargetState {
                    format: TextureFormat::bevy_default(),
                    blend: Some(BlendState::REPLACE),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: PrimitiveState {
                front_face: FrontFace::Ccw,
                cull_mode: Some(Face::Back),
                unclipped_depth: false,
                polygon_mode: PolygonMode::Fill,
                conservative: false,
                topology: PrimitiveTopology::TriangleList,
                strip_index_format: None,
            },
            depth_stencil: Some(DepthStencilState {
                format: TextureFormat::Depth32Float,
                depth_write_enabled: true,
                depth_compare: CompareFunction::Greater,
                stencil: StencilState {
                    front: StencilFaceState::IGNORE,
                    back: StencilFaceState::IGNORE,
                    read_mask: 0,
                    write_mask: 0,
                },
                bias: DepthBiasState {
                    constant: 0,
                    slope_scale: 0.0,
                    clamp: 0.0,
                },
            }),
            multisample: MultisampleState {
                count: Msaa::default().samples(),
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            push_constant_ranges: vec![],
        });

        Self {
            pipeline_id,
            view_layout,
            quads_layout,
        }
    }
}

type DrawQuads = (
    SetItemPipeline,
    SetQuadsViewBindGroup<0>,
    SetGpuQuadsBindGroup<1>,
    DrawVertexPulledQuads,
);

pub struct SetQuadsViewBindGroup<const I: usize>;
impl<const I: usize, P: PhaseItem> RenderCommand<P> for SetQuadsViewBindGroup<I> {
    type Param = SRes<GpuQuadsViewBindGroup>;
    type ViewWorldQuery = Read<ViewUniformOffset>;
    type ItemWorldQuery = ();

    #[inline]
    fn render<'w>(
        _item: &P,
        view_uniform_offset: ROQueryItem<'w, Self::ViewWorldQuery>,
        _entity: ROQueryItem<'w, Self::ItemWorldQuery>,
        view_bind_group: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        pass.set_bind_group(
            I,
            &view_bind_group.into_inner().bind_group,
            &[view_uniform_offset.offset],
        );

        RenderCommandResult::Success
    }
}

struct SetGpuQuadsBindGroup<const I: usize>;
impl<const I: usize, P: PhaseItem> RenderCommand<P> for SetGpuQuadsBindGroup<I> {
    type Param = SRes<GpuQuads>;
    type ViewWorldQuery = ();
    type ItemWorldQuery = ();

    #[inline]
    fn render<'w>(
        _item: &P,
        _view: ROQueryItem<'w, Self::ViewWorldQuery>,
        _entity: ROQueryItem<'w, Self::ItemWorldQuery>,
        gpu_quads: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        pass.set_bind_group(I, gpu_quads.into_inner().bind_group.as_ref().unwrap(), &[]);

        RenderCommandResult::Success
    }
}

struct DrawVertexPulledQuads;
impl<P: PhaseItem> RenderCommand<P> for DrawVertexPulledQuads {
    type Param = SRes<GpuQuads>;
    type ViewWorldQuery = ();
    type ItemWorldQuery = ();

    #[inline]
    fn render<'w>(
        _item: &P,
        _view: ROQueryItem<'w, Self::ViewWorldQuery>,
        _entity: ROQueryItem<'w, Self::ItemWorldQuery>,
        gpu_quads: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let gpu_quads = gpu_quads.into_inner();
        pass.set_index_buffer(
            gpu_quads.index_buffer.as_ref().unwrap().slice(..),
            0,
            IndexFormat::Uint32,
        );
        pass.draw_indexed(0..gpu_quads.index_count, 0, 0..1);
        RenderCommandResult::Success
    }
}

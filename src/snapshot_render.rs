use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::mpsc,
};

use fileman::{app_state, core, snapshot};

use crate::{
    ImageCache, ImageRequest, SNAPSHOT_HEIGHT, SNAPSHOT_WIDTH, UiCache, UiRender, draw_root_ui,
    hash_text, highlight_text_job,
};

use blade_egui as be;
use blade_graphics as bg;

pub(crate) fn render_snapshot(
    app: &mut app_state::AppState,
    ui_cache: &mut UiCache,
    path: &PathBuf,
) -> anyhow::Result<()> {
    let context = unsafe {
        bg::Context::init(bg::ContextDesc::default())
            .map_err(|err| anyhow::anyhow!("Failed to init GPU context: {err:?}"))?
    };

    let size = bg::Extent {
        width: SNAPSHOT_WIDTH,
        height: SNAPSHOT_HEIGHT,
        depth: 1,
    };
    let format = bg::TextureFormat::Rgba8Unorm;
    let surface_info = bg::SurfaceInfo {
        format,
        alpha: bg::AlphaMode::PreMultiplied,
    };
    let mut painter = be::GuiPainter::new(surface_info, &context);
    let mut command_encoder = context.create_command_encoder(bg::CommandEncoderDesc {
        name: "snapshot",
        buffer_count: 1,
    });

    let texture = context.create_texture(bg::TextureDesc {
        name: "snapshot_target",
        format,
        size,
        array_layer_count: 1,
        mip_level_count: 1,
        sample_count: 1,
        dimension: bg::TextureDimension::D2,
        usage: bg::TextureUsage::TARGET | bg::TextureUsage::COPY,
        external: None,
    });
    let view = context.create_texture_view(
        texture,
        bg::TextureViewDesc {
            name: "snapshot_view",
            format,
            dimension: bg::ViewDimension::D2,
            subresources: &bg::TextureSubresources::default(),
        },
    );

    let (preview_tx, _preview_req_rx) = mpsc::channel::<core::PreviewRequest>();
    let (_preview_content_tx, preview_rx) = mpsc::channel::<(u64, core::PreviewContent)>();
    let (io_tx, _io_rx_unused) = mpsc::channel::<core::IOTask>();
    let (_io_res_tx, io_rx) = mpsc::channel::<core::IOResult>();
    let (io_cancel_tx, _io_cancel_rx) = mpsc::channel::<()>();
    let (dir_size_tx, _dir_size_req_rx) = mpsc::channel::<(u64, Vec<PathBuf>)>();
    let (_dir_size_res_tx, dir_size_rx) =
        mpsc::channel::<(u64, Vec<fileman::workers::DirSizeNote>)>();
    let (edit_tx, _edit_req_rx) = mpsc::channel::<core::EditLoadRequest>();
    let (_edit_res_tx, edit_res_rx) = mpsc::channel::<core::EditLoadResult>();
    let (search_tx, _search_req_rx) = mpsc::channel::<core::SearchRequest>();
    let (_search_res_tx, search_rx) = mpsc::channel::<core::SearchEvent>();
    let (image_req_tx, _image_req_rx) = mpsc::channel::<ImageRequest>();
    let (highlight_req_tx, _highlight_req_rx) = mpsc::channel::<crate::HighlightRequest>();
    let mut image_cache = ImageCache {
        textures: HashMap::new(),
        animations: HashMap::new(),
        meta: HashMap::new(),
        failures: HashMap::new(),
        pending: HashSet::new(),
        refining: std::collections::HashMap::new(),
        order: VecDeque::new(),
    };
    let highlight_cache = build_snapshot_highlights(app);
    let mut highlight_pending = HashSet::new();

    app.preview_tx = preview_tx;
    app.preview_rx = preview_rx;
    app.io_tx = io_tx;
    app.io_rx = io_rx;
    app.io_cancel_tx = io_cancel_tx;
    app.dir_size_tx = dir_size_tx;
    app.dir_size_rx = dir_size_rx;
    app.edit_tx = edit_tx;
    app.edit_rx = edit_res_rx;
    app.search_tx = search_tx;
    app.search_rx = search_rx;
    app.refresh_tick = 0;

    let egui_ctx = egui::Context::default();
    let raw_input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::Vec2::new(SNAPSHOT_WIDTH as f32, SNAPSHOT_HEIGHT as f32),
        )),
        viewports: std::iter::once((
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                native_pixels_per_point: Some(1.0),
                inner_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::Vec2::new(SNAPSHOT_WIDTH as f32, SNAPSHOT_HEIGHT as f32),
                )),
                ..Default::default()
            },
        ))
        .collect(),
        ..Default::default()
    };
    let output = egui_ctx.run_ui(raw_input, |ui| {
        draw_root_ui(UiRender {
            ui,
            app,
            ui_cache,
            image_cache: &mut image_cache,
            image_req_tx: &image_req_tx,
            highlight_cache: &highlight_cache,
            highlight_pending: &mut highlight_pending,
            highlight_req_tx: &highlight_req_tx,
        });
    });

    let paint_jobs = egui_ctx.tessellate(output.shapes, output.pixels_per_point);
    let screen_descriptor = be::ScreenDescriptor {
        physical_size: (SNAPSHOT_WIDTH, SNAPSHOT_HEIGHT),
        scale_factor: 1.0,
    };

    command_encoder.start();
    command_encoder.init_texture(texture);
    painter.update_textures(&mut command_encoder, &output.textures_delta, &context);
    let mut render = command_encoder.render(
        "snapshot",
        bg::RenderTargetSet {
            colors: &[bg::RenderTarget {
                view,
                init_op: bg::InitOp::Clear(bg::TextureColor::TransparentBlack),
                finish_op: bg::FinishOp::Store,
            }],
            depth_stencil: None,
        },
    );
    painter.paint(&mut render, &paint_jobs, &screen_descriptor, &context);
    drop(render);

    let bytes_per_row = snapshot::align_to(SNAPSHOT_WIDTH * 4, 256);
    let buffer_size = bytes_per_row as u64 * SNAPSHOT_HEIGHT as u64;
    let result_buffer = context.create_buffer(bg::BufferDesc {
        name: "snapshot_readback",
        size: buffer_size,
        memory: bg::Memory::Shared,
    });

    {
        let mut transfer = command_encoder.transfer("snapshot readback");
        transfer.copy_texture_to_buffer(
            bg::TexturePiece {
                texture,
                mip_level: 0,
                array_layer: 0,
                origin: [0, 0, 0],
            },
            result_buffer.into(),
            bytes_per_row,
            size,
        );
    }

    let sync = context.submit(&mut command_encoder);
    painter.after_submit(&sync);
    context.wait_for(&sync, !0).ok();

    snapshot::save_snapshot_png(
        &result_buffer,
        SNAPSHOT_WIDTH,
        SNAPSHOT_HEIGHT,
        bytes_per_row as usize,
        path,
    )
    .map_err(|err| anyhow::anyhow!(err))?;

    context.destroy_texture_view(view);
    context.destroy_texture(texture);
    context.destroy_buffer(result_buffer);
    painter.destroy(&context);
    context.destroy_command_encoder(&mut command_encoder);

    Ok(())
}

fn build_snapshot_highlights(app: &app_state::AppState) -> HashMap<String, egui::text::LayoutJob> {
    let mut cache = HashMap::new();
    let theme_kind = app.theme.kind;

    if let Some(edit) = app.edit_panel()
        && let Some(path) = edit.path.as_ref()
    {
        let base_key = format!("edit:{}", path.to_string_lossy());
        let key = format!("{base_key}:{}", edit.highlight_hash);
        let job = highlight_text_job(&edit.text, edit.ext.as_deref(), theme_kind);
        cache.insert(key, job);
    }

    if let Some(preview) = app.preview_panel()
        && let Some(core::PreviewContent::Text(text)) = preview.content.as_ref()
    {
        let base_key = preview.key.clone().unwrap_or_else(|| "unknown".to_string());
        let key = format!("{base_key}:{:x}", hash_text(text));
        let job = highlight_text_job(text, preview.ext.as_deref(), theme_kind);
        cache.insert(key, job);
    }

    cache
}

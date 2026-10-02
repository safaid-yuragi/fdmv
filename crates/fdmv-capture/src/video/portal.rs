//! Linux: xdg-desktop-portal の画面共有（ScreenCast）＋ PipeWire。
//!
//! 画面共有ダイアログでユーザーが画面かウィンドウを選ぶと、PipeWire のストリームが渡される。
//! 専用スレッドで PipeWire のメインループを回し、届いたフレームを [`FrameSlot`] に置く。
//! ポータルのセッションは、そのスレッドが終わるまで開いたままにする。

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::Pod;

use super::{Frame, FrameSlot, PixelOrder, VideoCapture};

struct PortalCapture {
    slot: Arc<FrameSlot>,
    error: Arc<Mutex<Option<String>>>,
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl VideoCapture for PortalCapture {
    fn slot(&self) -> Arc<FrameSlot> {
        self.slot.clone()
    }

    fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// ダイアログを出し、選ばれたら取り込みを始める。ダイアログが閉じるまで戻らない。
pub fn start() -> Result<Box<dyn VideoCapture>> {
    let slot = Arc::new(FrameSlot::default());
    let error: Arc<Mutex<Option<String>>> = Arc::default();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<pw::channel::Sender<()>>>();
    let thread = {
        let (slot, error) = (slot.clone(), error.clone());
        std::thread::Builder::new()
            .name("fdmv-screencast".into())
            .spawn(move || {
                if let Err(e) = run(slot, ready_tx.clone()) {
                    let msg = format!("{e:#}");
                    // 開始前の失敗は呼び出し元に、開始後の失敗は error() で知らせる。
                    if ready_tx.send(Err(anyhow!(msg.clone()))).is_err() {
                        *error.lock().unwrap() = Some(msg);
                    }
                }
            })?
    };
    match ready_rx.recv() {
        Ok(Ok(quit)) => Ok(Box::new(PortalCapture {
            slot,
            error,
            quit: Some(quit),
            thread: Some(thread),
        })),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            bail!("画面共有を開始できませんでした")
        }
    }
}

fn run(slot: Arc<FrameSlot>, ready: mpsc::Sender<Result<pw::channel::Sender<()>>>) -> Result<()> {
    // ポータル（D-Bus）。セッションはこの関数を抜けるまで保持する。
    let (proxy, session, fd, node_id) = pollster::block_on(async {
        let proxy = Screencast::new()
            .await
            .context("画面共有のポータル（xdg-desktop-portal）に接続できません")?;
        let session = proxy.create_session(Default::default()).await?;
        let cursor = match proxy.available_cursor_modes().await {
            Ok(m) if m.contains(CursorMode::Embedded) => CursorMode::Embedded,
            _ => CursorMode::Hidden,
        };
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(cursor)
                    .set_sources(SourceType::Monitor | SourceType::Window)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await?
            .response()?;
        let response = proxy
            .start(&session, None, Default::default())
            .await?
            .response()
            .map_err(|e| match e {
                ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
                    anyhow!("画面の選択がキャンセルされました")
                }
                e => e.into(),
            })?;
        let stream = response
            .streams()
            .first()
            .ok_or_else(|| anyhow!("画面が選ばれませんでした"))?;
        let node_id = stream.pipe_wire_node_id();
        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await?;
        anyhow::Ok((proxy, session, fd, node_id))
    })?;

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_fd_rc(fd, None)
        .context("PipeWire に接続できません")?;

    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let _quit = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });

    let stream = pw::stream::StreamBox::new(
        &core,
        "fdmv-recorder",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(VideoInfoRaw::default())
        .param_changed(|_, format, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let _ = format.parse(param);
        })
        .process(move |stream, format| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let order = match format.format() {
                VideoFormat::BGRA => PixelOrder::Bgra,
                VideoFormat::BGRx => PixelOrder::Bgrx,
                VideoFormat::RGBA => PixelOrder::Rgba,
                VideoFormat::RGBx => PixelOrder::Rgbx,
                _ => return,
            };
            let size = format.size();
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let chunk = data.chunk();
            let (offset, len, stride) = (
                chunk.offset() as usize,
                chunk.size() as usize,
                chunk.stride(),
            );
            if len == 0 || chunk.as_raw().flags & 1 != 0 {
                // 中身の無いバッファ（カーソルだけの更新など）や壊れたバッファ。
                return;
            }
            let stride = if stride > 0 {
                stride as usize
            } else {
                size.width as usize * 4
            };
            let Some(bytes) = data.data() else { return };
            let Some(bytes) = bytes.get(offset..offset + len) else {
                return;
            };
            if let Some(frame) = Frame::from_strided(size.width, size.height, stride, bytes, order)
            {
                slot.put(frame);
            }
        })
        .register()?;

    let format = format_pod()?;
    let mut params = [Pod::from_bytes(&format).unwrap()];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    if ready.send(Ok(quit_tx)).is_err() {
        return Ok(());
    }
    mainloop.run();

    drop(stream);
    let _ = pollster::block_on(session.close());
    drop(proxy);
    Ok(())
}

/// 受け取れる形式: 32 bit の RGB 系（DMA-BUF の修飾子は付けない＝共有メモリで受け取る）。
fn format_pod() -> Result<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("PipeWire の形式を作れません: {e:?}"))?
    .0
    .into_inner())
}

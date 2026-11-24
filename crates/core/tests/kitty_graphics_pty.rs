// SPDX-License-Identifier: GPL-3.0-only
//! 真实 PTY 的 Kitty graphics 集成测试。
//!
//! 覆盖：跨 chunk APC、query APC 回包、chunked transmit + place、RowId 跨滚动、
//! C=1 不移动光标、delete placement/image、非法输入不破坏终端。

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use flate2::write::ZlibEncoder;
use flate2::Compression;

use xtervo_core::osc::encode_base64;
use xtervo_core::{spawn, Frame, PaneConfig, PaneHandle};

/// 同一进程里多个 PTY 用例并行会争抢 shell/CPU；串行化，风格与 sixel e2e 一致。
static KITTY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn sh(script: &str) -> PaneConfig {
    PaneConfig {
        cols: 40,
        rows: 10,
        scrollback: 100,
        shell: Some("/bin/bash".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        ..PaneConfig::default()
    }
}

fn frame_text(frame: &Frame) -> String {
    let mut out = String::with_capacity(frame.rows * (frame.cols + 1));
    for row in 0..frame.rows {
        let line: String = frame.row(row).iter().map(|c| c.char()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn wait_for<F: Fn(&Frame) -> bool>(handle: &PaneHandle, timeout: Duration, pred: F) -> Box<Frame> {
    let start = Instant::now();
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if pred(&frame) {
            return frame;
        }
        if start.elapsed() > timeout {
            panic!("超时等待 PTY 输出，当前屏幕:\n{}", frame_text(&frame));
        }
        std::thread::sleep(Duration::from_millis(3));
    }
}

fn temp_payload(name: &str, bytes: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("xtervo-kitty-{}-{name}.bin", std::process::id()));
    std::fs::write(&path, bytes).expect("write payload");
    path
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn apc(control: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(control.len() + payload.len() + 5);
    out.extend_from_slice(b"\x1b_G");
    out.extend_from_slice(control.as_bytes());
    out.push(b';');
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\x1b\\");
    out
}

/// 42 字节的 4x4 RGBA，base64 后按 8 字符切成多个 APC（m=1/m=0）。
fn chunked_kitty_place(command_id: u32, placement_id: u32, z: i32, no_move: bool) -> Vec<u8> {
    let width = 4u32;
    let height = 4u32;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for i in 0..width * height {
        rgba.extend_from_slice(&[(i * 7) as u8, 255 - i as u8, 0x40, 0xff]);
    }
    let encoded = encode_base64(&rgba);
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[5;3H"); // 1-based row 5, col 3 => grid (4, 2)
    for (index, chunk) in encoded.as_bytes().chunks(8).enumerate() {
        let more = usize::from(index + 1 < encoded.len().div_ceil(8));
        let control = if index == 0 {
            format!(
                "a=T,i={command_id},p={placement_id},f=32,s={width},v={height},c=2,r=2,x=0,y=0,w={width},h={height},z={z},C={},q=2,m={more}",
                if no_move { 1 } else { 0 }
            )
        } else {
            format!("m={more},q=2")
        };
        out.extend(apc(&control, chunk));
    }
    out
}

#[test]
fn kitty_query_over_pty_returns_apc_response() {
    let _serial = KITTY_TEST_LOCK.lock().unwrap();
    let expected = b"\x1b_Gi=31;OK\x1b\\".to_vec();
    let query = apc("i=31,s=1,v=1,a=q,t=d,f=32", b"AAAAAA==");
    let request = temp_payload("query", &query);
    let response = std::env::temp_dir().join(format!(
        "xtervo-kitty-{}-query-reply.bin",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&response);
    let script = format!(
        "stty raw -echo; cat '{}'; timeout --foreground 1 dd bs=1 count={} of='{}' 2>/dev/null || true; stty sane; printf '\\r\\nQRY-DONE\\r\\n'; sleep 0.3",
        request.display(),
        expected.len(),
        response.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");
    let _ = wait_for(&handle, Duration::from_secs(5), |f| {
        frame_text(f).contains("QRY-DONE")
    });
    let reply = std::fs::read(&response).unwrap_or_default();
    assert_eq!(reply, expected, "Kitty query response mismatch");
    let _ = std::fs::remove_file(request);
    let _ = std::fs::remove_file(response);
}

#[test]
fn kitty_transmit_place_follows_row_id_and_respects_c1() {
    let _serial = KITTY_TEST_LOCK.lock().unwrap();
    let mut payload = apc("i=31,s=1,v=1,a=q,t=d,f=32", b"AAAAAA==");
    payload.extend(chunked_kitty_place(7, 1, -1, true));
    payload.extend_from_slice(b"X");
    // 7 个 LF：从 grid 第 4 行走到第 11 行，viewport 10 行 => 滚动 2 行。
    for _ in 0..7 {
        payload.extend_from_slice(b"\r\n");
    }
    payload.extend_from_slice(b"END");
    let path = temp_payload("place", &payload);
    let script = format!("stty raw -echo; cat '{}'; sleep 0.6", path.display());
    let handle = spawn(sh(&script)).expect("spawn pane");

    let frame = wait_for(&handle, Duration::from_secs(5), |f| {
        f.images.len() == 1 && frame_text(f).contains("END")
    });
    assert!(handle.grid_lines().0 >= 2, "需要滚动历史来验证 RowId");
    let image = &frame.images[0];
    assert_eq!((image.width, image.height), (4, 4));
    assert_eq!(image.data.len(), 4 * 4 * 4);
    assert_eq!(image.z_index, -1);
    assert!(!image.above_text, "负 z 必须在文字之下");
    assert_eq!(image.row, 2, "4 - 2 行滚动");
    assert_eq!(frame.row_ids[image.row as usize], image.row_id);
    assert_ne!(image.row_id, 0);

    let meta = &frame.image_placements[0];
    assert_eq!(meta.placement_id, image.id);
    assert_eq!(meta.image_id, 7);
    assert_eq!(meta.row_id, image.row_id);
    assert_eq!(meta.cells_w, 2);
    assert_eq!(meta.cells_h, 2);
    assert_eq!(meta.z_index, -1);
    assert!(!meta.above_text);
    assert_eq!(
        meta.source_rect,
        Some(xtervo_core::frame::ImageCrop {
            x: 0,
            y: 0,
            w: 4,
            h: 4
        })
    );

    // C=1：光标仍在 (4,2)，所以 X 在滚动后位于 (2,2)。若错误地按 C=0
    // 右下移动 2x2，X 会出现在 (4,4)。
    assert_eq!(frame.row(2)[2].char(), 'X', "C=1 不得移动光标");

    // 点击/灯箱用 source_id 取原始 RGBA；Kitty 资源也必须能查到。
    let (w, h, data) = handle
        .image_rgba(image.source_id)
        .expect("kitty resource must be reachable by source_id");
    assert_eq!((w, h), (4, 4));
    assert_eq!(data.len(), 64);

    // 上滚 2 行：同一逻辑行下移 2 行，RowId 不变。
    let row_id = image.row_id;
    let old_row = image.row;
    handle.scroll(2);
    let frame = wait_for(&handle, Duration::from_secs(3), |f| {
        !f.images.is_empty() && f.images[0].row == old_row + 2
    });
    assert_eq!(frame.images[0].row_id, row_id);
    assert_eq!(frame.row_ids[frame.images[0].row as usize], row_id);

    // 回到底部。
    handle.scroll_to(0);
    let frame = wait_for(&handle, Duration::from_secs(3), |f| {
        !f.images.is_empty() && f.images[0].row == old_row
    });
    assert_eq!(frame.images[0].row_id, row_id);
    let _ = std::fs::remove_file(path);
}

#[test]
fn kitty_delete_placement_keeps_data_and_delete_image_drops_it() {
    let _serial = KITTY_TEST_LOCK.lock().unwrap();
    let mut place = Vec::new();
    place.extend_from_slice(b"\x1b[3;3H");
    place.extend(apc(
        "a=t,i=8,f=32,s=2,v=2,q=2",
        encode_base64(&[
            10u8, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ])
        .as_bytes(),
    ));
    place.extend(apc("a=p,i=8,p=1,c=1,r=1,q=2", b""));
    let place_path = temp_payload("delete-place", &place);
    let del_placement = temp_payload("delete-placement", &apc("a=d,d=i,i=8,p=1,q=2", b""));
    let del_image = temp_payload("delete-image", &apc("a=d,d=I,i=8,q=2", b""));
    let script = format!(
        "stty raw -echo; cat '{}'; dd bs=1 count=1 of=/dev/null 2>/dev/null; cat '{}'; printf '\\r\\nPLACEMENT-GONE\\r\\n'; dd bs=1 count=1 of=/dev/null 2>/dev/null; cat '{}'; printf 'IMAGE-GONE\\r\\n'; sleep 0.4",
        place_path.display(),
        del_placement.display(),
        del_image.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");

    let frame = wait_for(&handle, Duration::from_secs(5), |f| f.images.len() == 1);
    let source_id = frame.images[0].source_id;

    handle.input(b"a");
    let _ = wait_for(&handle, Duration::from_secs(5), |f| {
        f.images.is_empty() && frame_text(f).contains("PLACEMENT-GONE")
    });
    // 小写 d=i 只删 placement，像素数据仍可被灯箱/重放置使用。
    assert!(
        handle.image_rgba(source_id).is_some(),
        "d=i 不应释放图片数据"
    );

    handle.input(b"b");
    let _ = wait_for(&handle, Duration::from_secs(5), |f| {
        frame_text(f).contains("IMAGE-GONE")
    });
    assert!(
        handle.image_rgba(source_id).is_none(),
        "大写 d=I 必须释放图片数据"
    );

    let _ = std::fs::remove_file(place_path);
    let _ = std::fs::remove_file(del_placement);
    let _ = std::fs::remove_file(del_image);
}

#[test]
fn kitty_malformed_input_does_not_break_terminal() {
    let _serial = KITTY_TEST_LOCK.lock().unwrap();
    let mut payload = Vec::new();
    // 非法 base64。
    payload.extend(apc("a=t,i=1,s=1,v=1,f=32", b"!!!!"));
    // 未知 action。
    payload.extend(apc("a=z,i=2", b""));
    // 损坏的 zlib / 尺寸不符。
    payload.extend(apc(
        "a=t,i=3,s=1,v=1,f=32,o=z",
        encode_base64(b"not zlib").as_bytes(),
    ));
    // 未知 delete target。
    payload.extend(apc("a=d,d=?", b""));
    // 合法 query 仍然有回包，后续文本也照常。
    payload.extend(apc("i=31,s=1,v=1,a=q,t=d,f=32", b"AAAAAA=="));
    payload.extend_from_slice(b"ALIVE");
    let path = temp_payload("malformed", &payload);
    let script = format!(
        "stty raw -echo; cat '{}'; printf '\\r\\nMALFORMED-DONE\\r\\n'; sleep 0.4",
        path.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");

    let frame = wait_for(&handle, Duration::from_secs(5), |f| {
        frame_text(f).contains("MALFORMED-DONE")
    });
    assert!(frame.images.is_empty(), "非法输入不得生成图片");
    assert!(!handle.is_exited(), "非法 Kitty 输入不得让 actor 退出");
    let _ = std::fs::remove_file(path);
}

#[test]
fn kitty_compressed_rgba_transmit_and_positive_z() {
    let _serial = KITTY_TEST_LOCK.lock().unwrap();
    let rgba: Vec<u8> = vec![
        255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 255, 255, 0, 255,
    ];
    let compressed = zlib(&rgba);
    let encoded = encode_base64(&compressed);
    let mut payload = apc("a=t,i=9,f=32,s=2,v=2,o=z,q=2", encoded.as_bytes());
    payload.extend_from_slice(b"\x1b[3;3H");
    payload.extend(apc("a=p,i=9,p=2,c=1,r=1,z=1,C=1,q=2", b""));
    payload.extend_from_slice(b"Z");
    let path = temp_payload("compressed", &payload);
    let script = format!("stty raw -echo; cat '{}'; sleep 0.4", path.display());
    let handle = spawn(sh(&script)).expect("spawn pane");

    let frame = wait_for(&handle, Duration::from_secs(5), |f| {
        f.images.len() == 1 && frame_text(f).contains('Z')
    });
    let image = &frame.images[0];
    assert_eq!((image.width, image.height), (2, 2));
    assert_eq!(image.data.as_ref(), rgba.as_slice());
    assert_eq!(image.z_index, 1);
    assert!(image.above_text, "正 z 必须暴露 AboveText");
    assert_eq!(frame.image_placements[0].z_index, 1);
    assert!(frame.image_placements[0].above_text);
    // C=1：Z 仍在放置前的 (2,2)。
    assert_eq!(frame.row(2)[2].char(), 'Z');
    let _ = std::fs::remove_file(path);
}

/// 把字节转成 POSIX printf 格式串（ESC/BEL/反斜杠/百分号/单引号转义）。
fn printf_escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            0x1b => out.push_str("\\033"),
            0x07 => out.push_str("\\007"),
            b'\\' => out.push_str("\\\\"),
            b'%' => out.push_str("%%"),
            b'\'' => out.push_str("\\047"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{:03o}", b)),
        }
    }
    out
}

fn ok_reply(image: u32, placement: u32) -> Vec<u8> {
    match placement {
        0 => format!("\x1b_Gi={image};OK\x1b\\"),
        p => format!("\x1b_Gi={image},p={p};OK\x1b\\"),
    }
    .into_bytes()
}

fn enoent_reply(image: u32, placement: u32) -> Vec<u8> {
    format!("\x1b_Gi={image},p={placement};ENOENT:image not found\x1b\\").into_bytes()
}

fn transmit_1px(id: u32) -> Vec<u8> {
    apc(
        &format!("a=t,i={id},f=32,s=1,v=1,q=0"),
        encode_base64(&[1u8, 2, 3, 255]).as_bytes(),
    )
}

fn place_image(id: u32, placement: u32) -> Vec<u8> {
    apc(&format!("a=p,i={id},p={placement},c=1,r=1,q=0"), b"")
}

struct KittyRun {
    reply: Vec<u8>,
    images: usize,
}

/// 按 `chunks` 顺序写入终端（每块之间 sleep，确保 actor 分批读取），
/// 抓取终端回包与最终帧里的图片数量。
fn run_kitty_chunks(tag: &str, chunks: &[Vec<u8>], reply_len: usize) -> KittyRun {
    let dir = std::env::temp_dir().join(format!("xtervo-kitty-order-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let reply_path = dir.join("reply.bin");
    let mut script = String::from("stty raw -echo; ");
    for chunk in chunks {
        script.push_str(&format!("printf '{}'; sleep 0.15; ", printf_escape(chunk)));
    }
    script.push_str(&format!(
        "printf '\\r\\nDONE\\r\\n'; sleep 0.1; timeout --foreground 5 dd bs=1 count={reply_len} of='{}' 2>/dev/null || true; stty sane; sleep 0.2",
        reply_path.display()
    ));
    let handle = spawn(sh(&script)).expect("spawn pane");
    let frame = wait_for(&handle, Duration::from_secs(5), |f| {
        frame_text(f).contains("DONE")
    });
    let images = frame.images.len();
    let start = Instant::now();
    let reply = loop {
        let got = std::fs::read(&reply_path).unwrap_or_default();
        if got.len() >= reply_len || start.elapsed() > Duration::from_secs(6) {
            break got;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = std::fs::remove_dir_all(&dir);
    KittyRun { reply, images }
}

/// O2：同一 chunk 里先 `2J` 再 transmit/place，解析器的 image index 不能被
/// chunk 尾部的 clear 复位；后续 chunk 的 place 仍必须命中。
#[test]
fn kitty_clear_then_transmit_same_chunk_matches_split_chunks() {
    let _serial = KITTY_TEST_LOCK.lock().unwrap();
    let mut expected = ok_reply(41, 0);
    expected.extend(ok_reply(41, 1));
    expected.extend(ok_reply(41, 2));

    let mut same_chunk = b"\x1b[2J".to_vec();
    same_chunk.extend(transmit_1px(41));
    same_chunk.extend(place_image(41, 1));
    let same = run_kitty_chunks(
        "clear-first-same",
        &[same_chunk, place_image(41, 2)],
        expected.len(),
    );

    let split = run_kitty_chunks(
        "clear-first-split",
        &[
            b"\x1b[2J".to_vec(),
            [transmit_1px(41), place_image(41, 1)].concat(),
            place_image(41, 2),
        ],
        expected.len(),
    );

    assert_eq!(
        same.reply, expected,
        "同 chunk 清屏后 transmit/place 回包错误"
    );
    assert_eq!(split.reply, expected, "拆批对照回包错误");
    assert_eq!(same.images, 2, "同 chunk：清屏后的两个 place 都必须保留");
    assert_eq!(same.images, split.images, "同批与拆批图片数必须一致");
}

/// O2：同一 chunk 中 transmit 之后才 `2J`，图片必须已被清掉；之后的 place
/// 必须按清屏后的 parser index 返回 ENOENT，与拆成两个 chunk 的结果一致。
#[test]
fn kitty_transmit_then_clear_same_chunk_matches_split_chunks() {
    let _serial = KITTY_TEST_LOCK.lock().unwrap();
    let mut expected = ok_reply(42, 0);
    expected.extend(enoent_reply(42, 3));

    let mut same_chunk = transmit_1px(42);
    same_chunk.extend_from_slice(b"\x1b[2J");
    same_chunk.extend(place_image(42, 3));
    let same = run_kitty_chunks("clear-last-same", &[same_chunk], expected.len());

    let mut split_second = b"\x1b[2J".to_vec();
    split_second.extend(place_image(42, 3));
    let split = run_kitty_chunks(
        "clear-last-split",
        &[transmit_1px(42), split_second],
        expected.len(),
    );

    assert_eq!(
        same.reply, expected,
        "transmit 后清屏，后续 place 必须 ENOENT"
    );
    assert_eq!(split.reply, expected, "拆批对照回包错误");
    assert_eq!(same.images, 0, "清屏后不得残留图片");
    assert_eq!(same.images, split.images, "同批与拆批图片数必须一致");
}

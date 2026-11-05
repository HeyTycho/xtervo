// SPDX-License-Identifier: GPL-3.0-only
//! 字体加载耗时探针（release 下运行）。
//! cargo run --release -p tervo --example font_probe

use fontdue::{Font, FontSettings};
use std::time::Instant;

fn probe(path: &str, idx: u32) {
    let t = Instant::now();
    let Ok(bytes) = std::fs::read(path) else {
        println!("{path}#{idx}: 读取失败");
        return;
    };
    let read = t.elapsed();
    let mb = bytes.len() as f64 / 1024.0 / 1024.0;
    let t2 = Instant::now();
    match Font::from_bytes(bytes, FontSettings { collection_index: idx, ..Default::default() }) {
        Ok(f) => println!(
            "{path}#{idx}: {mb:.1}MB read={read:?} parse={:?} 你={} 好={} A={}",
            t2.elapsed(),
            f.has_glyph('你'),
            f.has_glyph('好'),
            f.has_glyph('A')
        ),
        Err(e) => println!("{path}#{idx}: 解析失败 {e:?}"),
    }
}

fn main() {
    probe("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 7);
    probe("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 2);
    probe("/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc", 7);
    probe("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", 1);
    probe("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0);
    probe("/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf", 0);
    probe("/usr/share/fonts/truetype/arphic/uming.ttc", 0);
}

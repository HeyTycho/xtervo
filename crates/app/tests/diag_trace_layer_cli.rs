// SPDX-License-Identifier: GPL-3.0-only
//! `tervo diag trace-layer` 的 CLI 契约：JSON schema、PASS、以及三种故障
//! 注入都必须落到正确的 layer/byte/row/col。用真实二进制而不是直接调
//! 模块，保证 `tervo diag ...` 的参数解析没有断。

use std::process::Command;

use serde_json::Value;

fn run(args: &[&str]) -> Value {
    let bin = env!("CARGO_BIN_EXE_tervo");
    let output = Command::new(bin)
        .arg("diag")
        .arg("trace-layer")
        .args(args)
        .output()
        .expect("spawn tervo");
    assert!(
        output.status.success(),
        "tervo diag trace-layer {args:?} 退出失败: status={:?}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "输出不是 JSON: {e}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn clean_fixture_is_pass_with_stable_schema() {
    let value = run(&["--fixture", "clean", "--json"]);
    assert_eq!(value["schema"], "tervo.diag.v1");
    assert_eq!(value["kind"], "trace_layer");
    assert_eq!(value["status"], "PASS");
    assert!(value["first_divergence"].is_null());
    assert_eq!(value["options"]["decode_images"], true);
    let layers = value["layers"].as_array().expect("layers array");
    let names: Vec<_> = layers.iter().filter_map(|l| l["layer"].as_str()).collect();
    for expected in ["filter", "term", "frame", "framebuffer", "image"] {
        assert!(
            names.contains(&expected),
            "缺少 layer {expected}: {names:?}"
        );
    }
    assert_eq!(value["status"], "PASS");
}

#[test]
fn injected_filter_drop_is_located_at_raw_byte() {
    // clean fixture 的 filtered 输出与 raw 一致直到 Sixel；丢掉 filtered[6]
    // 必须报 filter/byte_conservation，byte=6，left='e'。
    let value = run(&["--fixture", "clean", "--fault", "filter-drop=6", "--json"]);
    assert_eq!(value["status"], "FAIL", "{value}");
    let divergence = &value["first_divergence"];
    assert_eq!(divergence["layer"], "filter");
    assert_eq!(divergence["check"], "byte_conservation");
    assert_eq!(divergence["at"]["byte"], 6);
    assert_eq!(divergence["left"]["byte"], b'e');
    assert_eq!(divergence["right"]["missing"], true);
}

#[test]
fn injected_frame_residue_is_located_by_row_and_col() {
    let value = run(&[
        "--fixture",
        "clean",
        "--fault",
        "frame-residue=1:2@1",
        "--json",
    ]);
    assert_eq!(value["status"], "FAIL", "{value}");
    let divergence = &value["first_divergence"];
    assert_eq!(divergence["layer"], "frame");
    assert_eq!(divergence["check"], "partial_vs_force_full");
    assert_eq!(divergence["at"]["row"], 1);
    assert_eq!(divergence["at"]["col"], 2);
    assert!(divergence["left"]["cell"].as_str().unwrap().contains('#'));
}

#[test]
fn injected_damage_drop_is_located_as_framebuffer_residue() {
    let value = run(&[
        "--fixture",
        "clean",
        "--fault",
        "damage-drop=0",
        "--chunk",
        "7",
        "--json",
    ]);
    assert_eq!(value["status"], "FAIL", "{value}");
    let divergence = &value["first_divergence"];
    assert_eq!(divergence["layer"], "framebuffer");
    assert_eq!(divergence["check"], "damage_residue");
    assert_eq!(divergence["at"]["row"], 0);
}

#[test]
fn synthetic_filter_edge_fixture_passes() {
    let value = run(&["--fixture", "filter-edge", "--json"]);
    assert_eq!(value["status"], "PASS", "{value}");
    assert_eq!(value["layers"][0]["layer"], "filter");
    assert_eq!(value["layers"][0]["status"], "PASS");
}

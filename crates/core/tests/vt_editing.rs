// SPDX-License-Identifier: GPL-3.0-only
//! 编辑/滚动/制表位契约：IL/DL、DECSTBM、SU/SD、ICH/DCH/ECH、HTS/TBC/CHT/CBT、DECAWM。
//!
//! TERM-003 的专项验收。用 [`TermSim`]（无 PTY）逐字节喂入，断言可见网格与光标，
//! 避免 shell 启动噪声；这些序列是 vim/less/tmux/htop 的骨架操作，必须逐格钉死。
//!
//! 覆盖范围（上游 alacritty_terminal 实现 + 本项目管线）：
//! - `CSI L` / `CSI M`（IL/DL）与默认参数、超出滚动区时不动作；
//! - `CSI r`（DECSTBM）设置/复位滚动区，LF/IND 只在区内滚；
//! - `CSI S` / `CSI T`（SU/SD），滚动区外的行不动；
//! - `CSI @` / `CSI P` / `CSI X`（ICH/DCH/ECH）与右边界裁剪；
//! - `HTS`(ESC H) / `TBC`(CSI 0g,3g) / `CHT`(CSI I) / `CBT`(CSI Z)；
//! - `CSI ?7l/h`（DECAWM）关闭后不自动换行。
//!
//! 不覆盖：DECCOLM 132 列切换（alacritty 忽略）、双向/竖排终端、滚动区内的
//! DECOM 原点模式组合（另见 vt220_xterm.rs 的边界断言）。

use xtervo_core::TermSim;

fn sim(cols: usize, rows: usize) -> TermSim {
    TermSim::new(cols, rows, 100)
}

fn line(sim: &mut TermSim, row: usize) -> String {
    let frame = sim.pull();
    frame
        .row(row)
        .iter()
        .map(|c| c.char())
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn screen(sim: &mut TermSim) -> Vec<String> {
    let rows = sim.rows();
    (0..rows).map(|r| line(sim, r)).collect()
}

fn cursor(sim: &mut TermSim) -> (u16, u16) {
    let frame = sim.pull();
    (frame.cursor.line, frame.cursor.col)
}

/// 五行的确定内容（行 0..4），给编辑类用例做基线。
fn five_lines(sim: &mut TermSim) {
    sim.advance(b"1\r\n2\r\n3\r\n4\r\n5");
}

#[test]
fn il_inserts_blank_line_at_cursor() {
    let mut s = sim(20, 6);
    five_lines(&mut s);
    // 光标回第 2 行（1-based），插入一行：1,_,2,3,4
    s.advance(b"\x1b[2;1H\x1b[L");
    assert_eq!(screen(&mut s)[..6], ["1", "", "2", "3", "4", "5"]);
}

#[test]
fn il_default_is_one_and_multiple_counts_supported() {
    let mut s = sim(20, 6);
    five_lines(&mut s);
    s.advance(b"\x1b[W"); // 兼容老式 CSI W：alacritty 不实现，不应改变屏幕
    s.advance(b"\x1b[2;1H\x1b[2L");
    assert_eq!(screen(&mut s)[..6], ["1", "", "", "2", "3", "4"]);
}

#[test]
fn dl_deletes_line_at_cursor() {
    let mut s = sim(20, 6);
    five_lines(&mut s);
    s.advance(b"\x1b[3;1H\x1b[M");
    // 删掉第 3 行的 "3"：1,2,4,5,空
    assert_eq!(screen(&mut s)[..6], ["1", "2", "4", "5", "", ""]);
}

#[test]
fn dl_more_than_remaining_lines_clears_to_bottom() {
    let mut s = sim(20, 6);
    five_lines(&mut s);
    s.advance(b"\x1b[4;1H\x1b[99M");
    assert_eq!(screen(&mut s)[..6], ["1", "2", "3", "", "", ""]);
}

#[test]
fn il_dl_are_confined_to_scroll_region() {
    let mut s = sim(20, 8);
    five_lines(&mut s);
    // 滚动区 2..4（1-based），光标在第 3 行插入：区外第 1/5 行必须原样。
    s.advance(b"\x1b[2;4r\x1b[3;1H\x1b[L");
    assert_eq!(screen(&mut s)[..8], ["1", "2", "", "3", "5", "", "", ""]);
}

#[test]
fn decstbm_confines_linefeed_scrolling() {
    let mut s = sim(20, 8);
    five_lines(&mut s);
    s.advance(b"\x1b[2;4r");
    // 光标到区内底行再换行：区内上滚一行，"2" 离开屏幕，区外行 1 和 5 不动。
    s.advance(b"\x1b[4;1H\n");
    assert_eq!(screen(&mut s)[..8], ["1", "3", "4", "", "5", "", "", ""]);
}

#[test]
fn cursor_outside_scroll_region_does_not_scroll() {
    let mut s = sim(20, 8);
    five_lines(&mut s);
    // 区 2..4，光标移到第 6 行换行：只下移光标，不滚动屏幕。
    s.advance(b"\x1b[2;4r\x1b[6;1H\n");
    let screen_before = screen(&mut s);
    assert_eq!(screen_before[..7], ["1", "2", "3", "4", "5", "", ""]);
    assert_eq!(cursor(&mut s), (6, 0));
}

#[test]
fn decstbm_reset_restores_full_screen_scroll() {
    let mut s = sim(20, 8);
    five_lines(&mut s);
    // 复位后光标在区内底行（第 4 行）换行：滚动区恢复整屏，所以不滚；
    // 再到第 8 行（整屏底）换行，整屏 8 行滚动：1 被顶出，2..5 上移一行。
    s.advance(b"\x1b[2;4r\x1b[r\x1b[4;1H\n");
    assert_eq!(screen(&mut s)[..8], ["1", "2", "3", "4", "5", "", "", ""]);
    s.advance(b"\x1b[8;1H\n");
    assert_eq!(screen(&mut s)[..8], ["2", "3", "4", "5", "", "", "", ""]);
}

#[test]
fn su_sd_scroll_only_within_region() {
    let mut s = sim(20, 8);
    five_lines(&mut s);
    s.advance(b"\x1b[2;4r\x1b[2S");
    // 区 2..4 上滚 2 行：2,3 离开，4 到顶，底部两行空；区外 1/5 不动。
    assert_eq!(screen(&mut s)[..8], ["1", "4", "", "", "5", "", "", ""]);
    s.advance(b"\x1b[2T");
    // 再下滚 2 行：4 下移两行，顶部空两行，5 仍在区外。
    assert_eq!(screen(&mut s)[..8], ["1", "", "", "4", "5", "", "", ""]);
}

#[test]
fn ich_dch_ech_clip_at_right_margin() {
    let mut s = sim(12, 4);
    s.advance(b"abcdef");
    // ICH 2 在光标（第 2 列 = 'b' 的位置）插入两个空格，右侧顺移。
    s.advance(b"\x1b[1;2H\x1b[2@");
    assert_eq!(line(&mut s, 0), "a  bcdef");
    // DCH 2 删除光标处两个空格，回到原样。
    s.advance(b"\x1b[1;2H\x1b[2P");
    assert_eq!(line(&mut s, 0), "abcdef");
    // ECH 3 从第 4 列（0-based 3 = 'd'）起擦 3 格，不移动光标。
    s.advance(b"\x1b[1;4H\x1b[3X");
    assert_eq!(line(&mut s, 0), "abc");
    // 右边界裁剪：整行 12 格写满后，从第 10 列起 ICH 99 不越界、不 panic，
    // 被推出右边界的 j/k/l 消失。
    s.advance(b"\x1b[2J\x1b[Habcdefghijkl\x1b[1;10H\x1b[99@");
    let l = line(&mut s, 0);
    assert_eq!(l, "abcdefghi", "右侧被推出去的字符应被裁掉");
}

#[test]
fn default_tab_stops_are_every_eight_columns() {
    let mut s = sim(40, 3);
    s.advance(b"a\tb\tc");
    let l = line(&mut s, 0);
    let positions: Vec<usize> = ['a', 'b', 'c']
        .iter()
        .map(|c| l.find(*c).expect("char present"))
        .collect();
    assert_eq!(positions, vec![0, 8, 16], "默认 8 列制表位: {l:?}");
}

#[test]
fn hts_tbc_cht_cbt_roundtrip() {
    let mut s = sim(40, 3);
    // 清掉默认制表位，再在 3/10/20 列设置：1-based CSI 3G 等。
    s.advance(b"\x1b[3g");
    s.advance(b"\x1b[4G\x1bH\x1b[11G\x1bH\x1b[21G\x1bH\x1b[1G");
    // CHT 3：向右第 3 个制表位（3 → 10 → 20）；CBT 2：向左 20 → 10（含当前列左侧最近档）。
    s.advance(b"X\x1b[3IY\x1b[2ZZ");
    let l = line(&mut s, 0);
    let x = l.find('X').unwrap();
    let y = l.find('Y').unwrap();
    let z = l.find('Z').unwrap();
    assert_eq!((x, y, z), (0, 20, 10), "CHT/CBT 应落到 HTS 标记的列: {l:?}");
}

#[test]
fn tbc_zero_clears_stop_at_cursor() {
    let mut s = sim(40, 2);
    // 默认第 8 列是制表位；在第 8 列 TBC 0 后，HT 应从第 0 列跳到第 16 列。
    s.advance(b"\x1b[9G\x1b[0g\x1b[1G\t");
    assert_eq!(cursor(&mut s), (0, 16));
}

#[test]
fn cbt_clamps_to_left_margin() {
    let mut s = sim(40, 2);
    s.advance(b"\x1b[1;20H\x1b[99ZZ");
    let l = line(&mut s, 0);
    assert_eq!(l.find('Z'), Some(0), "CBT 越过左边界应停在首列: {l:?}");
}

#[test]
fn decawm_off_keeps_writing_on_last_column() {
    let mut s = sim(10, 3);
    s.advance(b"\x1b[?7l");
    s.advance(b"0123456789AB");
    assert_eq!(line(&mut s, 0), "012345678B", "关闭自动换行后末列被覆盖");
    assert_eq!(line(&mut s, 1), "");
    assert_eq!(cursor(&mut s), (0, 9));
    // 重新打开后恢复换行。
    s.advance(b"\x1b[?7h\r\n");
    s.advance(b"XYZ");
    assert_eq!(line(&mut s, 1), "XYZ");
}

#[test]
fn ind_ri_nel_behave_like_esc_sequences() {
    let mut s = sim(20, 4);
    s.advance(b"A\x1bDB\x1bMC\x1bED");
    let rows = screen(&mut s);
    // A 后在下一行写 B（IND），RI 回上一行写 C（覆盖 B 行），NEL 再下一行行首写 D。
    let text: String = rows.join("|");
    assert!(
        text.contains('A') && text.contains('C') && text.contains('D'),
        "{rows:?}"
    );
    // row0 = "AC"（RI 回到列 1 写 C），row1 = "DB"（NEL 回到列 0 写 D，B 仍在列 1）。
    assert_eq!(rows[0], "A C");
    assert_eq!(rows[1], "DB", "NEL 必须回到列 0");
}

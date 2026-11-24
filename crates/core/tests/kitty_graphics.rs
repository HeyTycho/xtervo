// SPDX-License-Identifier: GPL-3.0-only
//! Integration tests for the Kitty graphics APC parser.

use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use xtervo_core::kitty_graphics::{
    CompositePolicy, CursorMovement, DeleteTarget, KittyCommand, KittyErrorCode, KittyEvent,
    KittyParser, SourceRect, MAX_APC_BYTES,
};
use xtervo_core::osc::encode_base64;

fn apc(control: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(control.len() + payload.len() + 5);
    out.extend_from_slice(b"\x1b_G");
    out.extend_from_slice(control.as_bytes());
    out.push(b';');
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\x1b\\");
    out
}

fn parse_commands(events: &[KittyEvent]) -> Vec<&KittyCommand> {
    events
        .iter()
        .filter_map(|event| match event {
            KittyEvent::Command(command) => Some(command),
            _ => None,
        })
        .collect()
}

fn parse_errors(events: &[KittyEvent]) -> Vec<&xtervo_core::kitty_graphics::KittyError> {
    events
        .iter()
        .filter_map(|event| match event {
            KittyEvent::Error(error) => Some(error),
            _ => None,
        })
        .collect()
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn rgba_pixels(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..width * height {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.extend_from_slice(&state.to_le_bytes());
    }
    out
}

fn chunked_transmission(control_first: &str, control_cont: &str, payload: &[u8]) -> Vec<Vec<u8>> {
    let encoded = encode_base64(payload);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();
    assert!(chunks.len() > 1, "test image must span multiple APCs");
    chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let more = if index + 1 < chunks.len() { 1 } else { 0 };
            if index == 0 {
                apc(&format!("{control_first},m={more}"), chunk)
            } else {
                apc(&format!("{control_cont},m={more}"), chunk)
            }
        })
        .collect()
}

#[test]
fn query_is_split_at_every_byte_boundary() {
    let query = apc("i=31,s=1,v=1,a=q,t=d,f=32", b"AAAAAA==");
    let expected = b"\x1b_Gi=31;OK\x1b\\".to_vec();

    for split in 0..=query.len() {
        let mut parser = KittyParser::new();
        let mut events = Vec::new();
        parser.feed_into(&query[..split], &mut events);
        parser.feed_into(&query[split..], &mut events);

        assert_eq!(events.len(), 1, "split at {split}");
        match &events[0] {
            KittyEvent::Command(KittyCommand::Response(response)) => {
                assert_eq!(response, &expected, "split at {split}");
            }
            other => panic!("unexpected event at split {split}: {other:?}"),
        }
        assert_eq!(parser.tracked_image_count(), 0);
        assert!(!parser.has_pending_transmission());
    }
}

#[test]
fn non_apc_bytes_are_passed_through_in_order() {
    let mut parser = KittyParser::new();
    let mut input = b"before".to_vec();
    input.extend(apc("i=31,s=1,v=1,a=q,t=d,f=32", b"AAAAAA=="));
    input.extend_from_slice(b"after");

    let events = parser.feed(&input);
    assert_eq!(events.len(), 3);
    match &events[0] {
        KittyEvent::Pass(bytes) => assert_eq!(bytes, b"before"),
        other => panic!("unexpected first event: {other:?}"),
    }
    match &events[1] {
        KittyEvent::Command(KittyCommand::Response(response)) => {
            assert_eq!(response, b"\x1b_Gi=31;OK\x1b\\");
        }
        other => panic!("unexpected second event: {other:?}"),
    }
    match &events[2] {
        KittyEvent::Pass(bytes) => assert_eq!(bytes, b"after"),
        other => panic!("unexpected third event: {other:?}"),
    }
}

#[test]
fn non_kitty_apc_is_passed_through_and_does_not_start_a_kitty_command() {
    let mut parser = KittyParser::new();
    let input = b"\x1b_SomeOtherApp\x1b\\hello";
    let events = parser.feed(input);
    assert_eq!(events.len(), 1);
    match &events[0] {
        KittyEvent::Pass(bytes) => assert_eq!(bytes, input),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn compressed_rgba_chunks_inherit_control_and_emit_one_transmit() {
    let (width, height) = (128u32, 128u32);
    let rgba = rgba_pixels(width, height, 7);
    let compressed = zlib(&rgba);
    let first = format!("a=t,i=7,f=32,s={width},v={height},o=z,q=2");
    let apcs = chunked_transmission(&first, "q=2", &compressed);
    assert!(apcs.len() > 1);

    let mut parser = KittyParser::new();
    let mut accumulated = Vec::new();
    for (index, apc) in apcs.iter().enumerate() {
        let events = parser.feed(apc);
        let count = parse_commands(&events).len();
        if index + 1 < apcs.len() {
            assert_eq!(count, 0, "must not emit before m=0 (chunk {index})");
            assert!(parser.has_pending_transmission());
        } else {
            accumulated.extend(events);
        }
    }

    assert!(!parser.has_pending_transmission());
    assert_eq!(parser.image_dimensions(7), Some((width, height)));
    let commands = parse_commands(&accumulated);
    let transmit = commands
        .iter()
        .find_map(|command| match command {
            KittyCommand::Transmit(transmit) => Some(transmit.clone()),
            _ => None,
        })
        .expect("final chunk must emit a transmit");
    assert_eq!(transmit.image.id, 7);
    assert_eq!(transmit.image.width, width);
    assert_eq!(transmit.image.height, height);
    assert_eq!(transmit.image.rgba, rgba);
    assert!(!transmit.replaces_existing);
    assert_eq!(transmit.quiet, 2);
    // q=2 suppresses every reply, so the chunked transmit is silent.
    assert_eq!(parse_errors(&accumulated).len(), 0);
    assert!(!accumulated
        .iter()
        .any(|event| matches!(event, KittyEvent::Command(KittyCommand::Response(_)))));

    let place = apc(
        "a=p,i=7,p=3,c=8,r=4,x=4,y=8,w=16,h=16,z=-2147483648,C=1,q=1",
        b"",
    );
    let events = parser.feed(&place);
    let commands = parse_commands(&events);
    assert_eq!(commands.len(), 1);
    match commands[0] {
        KittyCommand::Place(place) => {
            assert_eq!(place.image_id, 7);
            assert_eq!(place.placement_id, 3);
            assert_eq!(
                place.source_rect,
                Some(SourceRect {
                    x: 4,
                    y: 8,
                    w: 16,
                    h: 16
                })
            );
            assert_eq!(place.columns, 8);
            assert_eq!(place.rows, 4);
            assert_eq!(place.z_index, i32::MIN);
            assert_eq!(place.cursor_movement, CursorMovement::NoMove);
            assert_eq!(place.composite, CompositePolicy::BelowText);
            assert_eq!(place.quiet, 1);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    // q=1 suppresses the placement OK response.
    assert!(!events
        .iter()
        .any(|event| matches!(event, KittyEvent::Command(KittyCommand::Response(_)))));
}

#[test]
fn raw_rgba_and_rgb_transmit() {
    let mut parser = KittyParser::new();
    let rgba = [255u8, 0, 0, 255];
    let events = parser.feed(&apc(
        "a=t,i=1,s=1,v=1,f=32",
        encode_base64(&rgba).as_bytes(),
    ));
    match parse_commands(&events)[0] {
        KittyCommand::Transmit(transmit) => {
            assert_eq!(transmit.image.rgba, rgba);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let events = parser.feed(&apc(
        "a=t,i=3,s=1,v=1,f=24",
        encode_base64(&[0u8, 255, 0]).as_bytes(),
    ));
    match parse_commands(&events)[0] {
        KittyCommand::Transmit(transmit) => {
            assert_eq!(transmit.image.rgba, [0u8, 255, 0, 255]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn transmit_and_display_emits_transmit_then_place() {
    let mut parser = KittyParser::new();
    let data = [
        10u8, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
    ];
    let events = parser.feed(&apc(
        "a=T,i=12,p=5,f=32,s=2,v=2,c=1,r=1,z=0,C=1",
        encode_base64(&data).as_bytes(),
    ));

    let commands = parse_commands(&events);
    assert_eq!(commands.len(), 3);
    match commands[0] {
        KittyCommand::Transmit(transmit) => {
            assert_eq!(transmit.image.id, 12);
            assert_eq!(transmit.image.rgba, data);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    match commands[1] {
        KittyCommand::Place(place) => {
            assert_eq!(place.image_id, 12);
            assert_eq!(place.placement_id, 5);
            assert_eq!(place.composite, CompositePolicy::AboveText);
            assert_eq!(place.cursor_movement, CursorMovement::NoMove);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    // q defaults to 0: display acknowledgement with image and placement id.
    match commands[2] {
        KittyCommand::Response(response) => {
            assert_eq!(response, b"\x1b_Gi=12,p=5;OK\x1b\\");
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn delete_placement_and_image_targets() {
    let mut parser = KittyParser::new();
    let mut events = parser.feed(&apc(
        "a=t,i=9,f=32,s=2,v=2,q=1",
        encode_base64(&rgba_pixels(2, 2, 1)).as_bytes(),
    ));
    assert_eq!(parse_commands(&events).len(), 1);
    events.clear();

    events = parser.feed(&apc("a=d,d=i,i=9,p=4,q=2", b""));
    match parse_commands(&events)[0] {
        KittyCommand::Delete(delete) => {
            assert_eq!(
                delete.target,
                DeleteTarget::Image {
                    image_id: 9,
                    placement_id: Some(4),
                    free_data: false,
                }
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }
    // Lowercase d=i keeps the image data around for re-placement.
    assert_eq!(parser.image_dimensions(9), Some((2, 2)));

    events = parser.feed(&apc("a=d,d=I,i=9,q=2", b""));
    match parse_commands(&events)[0] {
        KittyCommand::Delete(delete) => {
            assert_eq!(
                delete.target,
                DeleteTarget::Image {
                    image_id: 9,
                    placement_id: None,
                    free_data: true,
                }
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }
    assert_eq!(parser.image_dimensions(9), None);

    events = parser.feed(&apc("a=d,d=a", b""));
    assert_eq!(
        parse_commands(&events)[0],
        &KittyCommand::Delete(xtervo_core::kitty_graphics::DeleteCommand {
            target: DeleteTarget::AllPlacements { free_data: false },
            quiet: 0,
        })
    );

    events = parser.feed(&apc("a=d", b""));
    assert_eq!(
        parse_commands(&events)[0],
        &KittyCommand::Delete(xtervo_core::kitty_graphics::DeleteCommand {
            target: DeleteTarget::All,
            quiet: 0,
        })
    );

    events = parser.feed(&apc("a=d,d=Z,z=-1,q=2", b""));
    assert_eq!(
        parse_commands(&events)[0],
        &KittyCommand::Delete(xtervo_core::kitty_graphics::DeleteCommand {
            target: DeleteTarget::ZIndex {
                z_index: -1,
                free_data: true,
            },
            quiet: 2,
        })
    );
}

#[test]
fn placement_of_missing_image_reports_enoent() {
    let mut parser = KittyParser::new();
    let events = parser.feed(&apc("a=p,i=77,p=2,q=0", b""));
    let errors = parse_errors(&events);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, KittyErrorCode::NoSuchImage);
    assert_eq!(errors[0].image_id, Some(77));
    assert_eq!(
        errors[0].response.as_deref(),
        Some(b"\x1b_Gi=77,p=2;ENOENT:image not found\x1b\\".as_slice())
    );

    // q=2 suppresses the error response but still reports the parse error.
    let events = parser.feed(&apc("a=p,i=78,q=2", b""));
    let errors = parse_errors(&events);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, KittyErrorCode::NoSuchImage);
    assert!(errors[0].response.is_none());
}

#[test]
fn malformed_input_is_rejected_without_panicking() {
    let mut parser = KittyParser::new();

    let events = parser.feed(&apc("a=t,i=1,s=1,v=1,f=32", b"!!!!"));
    let errors = parse_errors(&events);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, KittyErrorCode::BadBase64);
    assert!(errors[0]
        .response
        .as_deref()
        .is_some_and(|response| response.starts_with(b"\x1b_Gi=1;EINVAL:")));

    let events = parser.feed(&apc(
        "a=t,i=2,s=1,v=1,f=32,o=z",
        encode_base64(b"not a zlib stream").as_bytes(),
    ));
    assert_eq!(parse_errors(&events)[0].code, KittyErrorCode::BadZlib);

    let events = parser.feed(&apc("a=t,i=3,s=0,v=1,f=32", b""));
    assert_eq!(parse_errors(&events)[0].code, KittyErrorCode::Invalid);

    // 8192 x 8192 RGBA is 256 MiB, above MAX_IMAGE_BYTES.
    let events = parser.feed(&apc("a=t,i=4,s=8192,v=8192,f=32", b""));
    assert_eq!(parse_errors(&events)[0].code, KittyErrorCode::TooLarge);

    // Duplicate control keys are ambiguous.
    let events = parser.feed(&apc("a=t,i=5,i=6,s=1,v=1,f=32", b""));
    assert_eq!(parse_errors(&events)[0].code, KittyErrorCode::Invalid);

    // Payload size must match the declared dimensions exactly.
    let events = parser.feed(&apc(
        "a=t,i=7,s=2,v=2,f=32",
        encode_base64(&[0u8; 4]).as_bytes(),
    ));
    assert_eq!(parse_errors(&events)[0].code, KittyErrorCode::Invalid);

    // PNG is not implemented: report it instead of faking success.
    let events = parser.feed(&apc("a=q,i=8,s=1,v=1,f=100", b"AAAA"));
    let errors = parse_errors(&events);
    assert_eq!(errors[0].code, KittyErrorCode::Unsupported);
    assert!(errors[0]
        .response
        .as_deref()
        .is_some_and(|response| response.starts_with(b"\x1b_Gi=8;ENOTSUP:")));

    // Mismatched continuation chunks report one error and recover.
    let mut events = parser.feed(&apc("a=t,i=9,s=1,v=1,f=32,m=1", b"AA"));
    assert!(parse_errors(&events).is_empty());
    assert!(parser.has_pending_transmission());
    events = parser.feed(&apc("m=0", b""));
    assert_eq!(parse_errors(&events)[0].code, KittyErrorCode::BadBase64);
    assert!(!parser.has_pending_transmission());
    let events = parser.feed(&apc("i=31,s=1,v=1,a=q,t=d,f=32", b"AAAAAA=="));
    assert!(events.iter().any(|event| matches!(
        event,
        KittyEvent::Command(KittyCommand::Response(response))
            if response == b"\x1b_Gi=31;OK\x1b\\"
    )));
}

#[test]
fn oversized_apc_is_rejected_and_parser_recovers() {
    let mut parser = KittyParser::new();
    let mut oversized = Vec::with_capacity(MAX_APC_BYTES + 16);
    oversized.extend_from_slice(b"\x1b_Ga=t,i=1,s=1,v=1,f=32;");
    oversized.extend(std::iter::repeat_n(b'A', MAX_APC_BYTES + 1));
    oversized.extend_from_slice(b"\x1b\\");

    let events = parser.feed(&oversized);
    let errors = parse_errors(&events);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, KittyErrorCode::TooLarge);
    assert!(!parser.has_pending_transmission());

    let events = parser.feed(&apc("i=31,s=1,v=1,a=q,t=d,f=32", b"AAAAAA=="));
    assert!(events.iter().any(|event| matches!(
        event,
        KittyEvent::Command(KittyCommand::Response(response))
            if response == b"\x1b_Gi=31;OK\x1b\\"
    )));
}

#[test]
fn all_byte_values_are_safe_to_feed() {
    let mut parser = KittyParser::new();
    for byte in 0u8..=255 {
        let _ = parser.feed(&[byte]);
    }
    let _ = parser.feed(b"\x1b\\");
    parser.reset();
    assert!(!parser.has_pending_transmission());
    assert_eq!(parser.tracked_image_count(), 0);
}

#[test]
fn invalid_source_rectangle_is_rejected() {
    let mut parser = KittyParser::new();
    let _ = parser.feed(&apc(
        "a=t,i=5,f=32,s=4,v=2,q=2",
        encode_base64(&rgba_pixels(4, 2, 2)).as_bytes(),
    ));
    let events = parser.feed(&apc("a=p,i=5,x=3,y=0,w=2,h=1,q=1", b""));
    let errors = parse_errors(&events);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, KittyErrorCode::Invalid);
    assert!(errors[0]
        .response
        .as_deref()
        .is_some_and(|response| response.starts_with(b"\x1b_Gi=5;EINVAL:")));
}

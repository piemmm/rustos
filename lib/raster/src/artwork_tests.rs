//! Unit tests for the shared artwork tree: layer counting, group isolation,
//! the two mask kinds, and the fail-closed refusals.

use alloc::vec;
use alloc::vec::Vec;

use super::{layer_count, Group, Layer, Mask, MaskKind, Node, MAX_GROUP_DEPTH};
use crate::color::{Color, Pixel};
use crate::paint::Paint;
use crate::scan::FillRule;
use crate::surface::Surface;

/// A full-grid square in `color`.
fn square(color: Color) -> Node {
    Node::Fill(Layer::filled(
        Paint::Solid(color),
        FillRule::NonZero,
        vec![vec![(0, 0), (16, 0), (16, 16), (0, 16)]],
    ))
}

/// The left half of the grid in `color`.
fn left_half(color: Color) -> Node {
    Node::Fill(Layer::filled(
        Paint::Solid(color),
        FillRule::NonZero,
        vec![vec![(0, 0), (8, 0), (8, 16), (0, 16)]],
    ))
}

/// Draw `nodes` on a fresh 16×16 surface over a 16-unit design grid.
fn drawn(nodes: &[Node]) -> (Surface, bool) {
    let mut surface = Surface::new(16, 16).expect("a 16×16 surface");
    let whole = surface.draw_artwork(nodes, 16);
    (surface, whole)
}

#[test]
fn a_flat_stack_paints_bottom_first() {
    let (surface, whole) = drawn(&[
        square(Color::rgb(255, 0, 0)),
        left_half(Color::rgb(0, 0, 255)),
    ]);
    assert!(whole);
    assert_eq!(surface.get(2, 8), Some(Color::rgb(0, 0, 255).premultiply()));
    assert_eq!(
        surface.get(12, 8),
        Some(Color::rgb(255, 0, 0).premultiply())
    );
}

#[test]
fn a_transparent_group_draws_nothing_and_is_complete() {
    let (surface, whole) = drawn(&[Node::Group(Group {
        opacity: 0,
        mask: None,
        children: vec![square(Color::rgb(255, 0, 0))],
    })]);
    assert!(whole);
    assert_eq!(surface.get(8, 8), Some(Pixel::TRANSPARENT));
}

#[test]
fn a_full_opacity_group_draws_as_its_children_do() {
    let children = vec![
        square(Color::rgb(255, 0, 0)),
        left_half(Color::rgb(0, 0, 255)),
    ];
    let (flat, _) = drawn(&children);
    let (grouped, whole) = drawn(&[Node::Group(Group {
        opacity: 255,
        mask: None,
        children,
    })]);
    assert!(whole);
    assert_eq!(grouped.pixels(), flat.pixels());
}

/// Isolation is the whole point: compositing the subtree and then weakening
/// it is a different picture from weakening each layer and compositing.
#[test]
fn group_opacity_isolates_rather_than_weakening_each_layer() {
    let red = Color::rgb(255, 0, 0);
    let blue = Color::rgb(0, 0, 255);
    let (isolated, _) = drawn(&[Node::Group(Group {
        opacity: 128,
        mask: None,
        children: vec![square(red), square(blue)],
    })]);
    let (per_layer, _) = drawn(&[
        square(Color::rgba(255, 0, 0, 128)),
        square(Color::rgba(0, 0, 255, 128)),
    ]);
    // The upper layer is opaque, so the isolated group shows only it.
    let pixel = isolated.get(8, 8).expect("a drawn pixel");
    assert_eq!(pixel, Color::rgba(0, 0, 255, 128).premultiply());
    assert_ne!(per_layer.get(8, 8), Some(pixel));
}

#[test]
fn an_alpha_mask_clips_to_its_content() {
    let (surface, whole) = drawn(&[Node::Group(Group {
        opacity: 255,
        mask: Some(Mask {
            kind: MaskKind::Alpha,
            content: vec![left_half(Color::rgb(255, 255, 255))],
        }),
        children: vec![square(Color::rgb(255, 0, 0))],
    })]);
    assert!(whole);
    assert_eq!(surface.get(2, 8), Some(Color::rgb(255, 0, 0).premultiply()));
    assert_eq!(surface.get(12, 8), Some(Pixel::TRANSPARENT));
}

/// Two shapes in one alpha mask union, because opaque over opaque is opaque —
/// which is what makes a multi-shape clip path need no second rule.
#[test]
fn overlapping_alpha_mask_shapes_union() {
    let white = Color::rgb(255, 255, 255);
    let (surface, _) = drawn(&[Node::Group(Group {
        opacity: 255,
        mask: Some(Mask {
            kind: MaskKind::Alpha,
            content: vec![left_half(white), left_half(white)],
        }),
        children: vec![square(Color::rgb(0, 255, 0))],
    })]);
    assert_eq!(surface.get(2, 8), Some(Color::rgb(0, 255, 0).premultiply()));
}

#[test]
fn a_luminance_mask_weighs_the_content_colour() {
    let (white, _) = drawn(&[Node::Group(Group {
        opacity: 255,
        mask: Some(Mask {
            kind: MaskKind::Luminance,
            content: vec![square(Color::rgb(255, 255, 255))],
        }),
        children: vec![square(Color::rgb(255, 0, 0))],
    })]);
    let (black, _) = drawn(&[Node::Group(Group {
        opacity: 255,
        mask: Some(Mask {
            kind: MaskKind::Luminance,
            content: vec![square(Color::rgb(0, 0, 0))],
        }),
        children: vec![square(Color::rgb(255, 0, 0))],
    })]);
    assert_eq!(white.get(8, 8), Some(Color::rgb(255, 0, 0).premultiply()));
    assert_eq!(black.get(8, 8), Some(Pixel::TRANSPARENT));
}

/// Opaque red is well short of half-lit under the sRGB weights, so a red
/// luminance mask must not be read as a plain alpha one.
#[test]
fn luminance_and_alpha_read_the_same_content_differently() {
    let content = vec![square(Color::rgb(255, 0, 0))];
    let subject = vec![square(Color::rgb(0, 0, 255))];
    let by_alpha = MaskKind::Alpha.factor(Color::rgb(255, 0, 0).premultiply());
    let by_luma = MaskKind::Luminance.factor(Color::rgb(255, 0, 0).premultiply());
    assert_eq!(by_alpha, 255);
    assert_eq!(by_luma, 54);
    let (surface, _) = drawn(&[Node::Group(Group {
        opacity: 255,
        mask: Some(Mask {
            kind: MaskKind::Luminance,
            content,
        }),
        children: subject,
    })]);
    assert_eq!(
        surface.get(8, 8),
        Some(Color::rgba(0, 0, 255, by_luma).premultiply())
    );
}

/// A premultiplied pixel's luminance already carries its alpha, so a
/// half-transparent white masks to half.
#[test]
fn luminance_carries_the_content_alpha() {
    let half = Color::rgba(255, 255, 255, 128).premultiply();
    assert_eq!(MaskKind::Luminance.factor(half), 128);
}

#[test]
fn a_mask_nested_in_a_mask_composes() {
    let white = Color::rgb(255, 255, 255);
    let top_left = Node::Fill(Layer::filled(
        Paint::Solid(white),
        FillRule::NonZero,
        vec![vec![(0, 0), (16, 0), (16, 8), (0, 8)]],
    ));
    let (surface, whole) = drawn(&[Node::Group(Group {
        opacity: 255,
        mask: Some(Mask {
            kind: MaskKind::Alpha,
            content: vec![Node::Group(Group {
                opacity: 255,
                mask: Some(Mask {
                    kind: MaskKind::Alpha,
                    content: vec![top_left],
                }),
                children: vec![left_half(white)],
            })],
        }),
        children: vec![square(Color::rgb(255, 0, 0))],
    })]);
    assert!(whole);
    assert_eq!(surface.get(2, 2), Some(Color::rgb(255, 0, 0).premultiply()));
    assert_eq!(surface.get(2, 12), Some(Pixel::TRANSPARENT));
    assert_eq!(surface.get(12, 2), Some(Pixel::TRANSPARENT));
}

/// Nesting past the bound is refused whole, and nothing of it is drawn.
#[test]
fn nesting_past_the_bound_is_refused_and_draws_nothing() {
    let mut node = square(Color::rgb(255, 0, 0));
    for _ in 0..=MAX_GROUP_DEPTH {
        node = Node::Group(Group {
            opacity: 255,
            mask: None,
            children: vec![node],
        });
    }
    let (surface, whole) = drawn(&[node]);
    assert!(!whole);
    assert!(surface.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

#[test]
fn layer_count_sees_into_groups_and_masks() {
    let nodes = vec![
        square(Color::rgb(0, 0, 0)),
        Node::Group(Group {
            opacity: 128,
            mask: Some(Mask {
                kind: MaskKind::Alpha,
                content: vec![square(Color::rgb(255, 255, 255))],
            }),
            children: vec![square(Color::rgb(1, 2, 3)), square(Color::rgb(4, 5, 6))],
        }),
    ];
    assert_eq!(layer_count(&nodes), 4);
}

#[test]
fn a_group_honours_the_enclosing_clip_window() {
    let mut surface = Surface::new(16, 16).expect("a 16×16 surface");
    surface.with_clip(0, 0, 8, 16, |surface| {
        assert!(surface.draw_artwork(
            &[Node::Group(Group {
                opacity: 255,
                mask: None,
                children: vec![square(Color::rgb(255, 0, 0))],
            })],
            16,
        ));
    });
    assert_eq!(surface.get(2, 8), Some(Color::rgb(255, 0, 0).premultiply()));
    assert_eq!(surface.get(12, 8), Some(Pixel::TRANSPARENT));
}

/// A group drawn through a stated origin lands where the same artwork lands
/// on the whole drawing, which is what a banded render depends on.
#[test]
fn a_group_lands_where_the_whole_drawing_puts_it() {
    let nodes = vec![Node::Group(Group {
        opacity: 128,
        mask: Some(Mask {
            kind: MaskKind::Alpha,
            content: vec![left_half(Color::rgb(255, 255, 255))],
        }),
        children: vec![square(Color::rgb(0, 128, 255))],
    })];
    let mut whole = Surface::new(16, 16).expect("a 16×16 surface");
    assert!(whole.draw_artwork(&nodes, 16));

    let mut strip = Surface::new(16, 4).expect("a 16×4 strip");
    strip.with_origin(0, 8, |surface| {
        assert!(surface.draw_artwork_over(
            crate::resample::Region {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
            },
            &nodes,
            16,
        ));
    });
    let band: Vec<_> = whole.pixels()[8 * 16..12 * 16].to_vec();
    assert_eq!(strip.pixels(), band.as_slice());
}

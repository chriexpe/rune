//! `Tx::link_rect`: an `on_link` handler is told where the pressed link is drawn, so what it
//! opens can sit beside the link; other handlers are told nothing.
use super::*;
use crate::{Cx, El, Env, Link, TextStyle, col, rich};

/// The box each `on_link` call was given, and what a press handler was.
#[derive(Default)]
struct S {
    links: Vec<Option<[f32; 4]>>,
    press: Option<Option<[f32; 4]>>,
}

const CONTENT: &str = "read the docs now";

fn runtime() -> Runtime<S> {
    fn view(_: &S, _: &mut Cx<S>) -> El {
        let docs = CONTENT.find("docs").unwrap();
        col()
            .w_full()
            .child(
                rich(CONTENT, Vec::new())
                    .key("t")
                    .font(TextStyle::new(13.0, 400.0).lh(16.0))
                    .link(Link::new(docs..docs + 4, "docs"))
                    .on_link(|s: &mut S, _, tx| s.links.push(tx.link_rect())),
            )
            .child(
                crate::el()
                    .key("b")
                    .size(40.0, 20.0)
                    .on_press_tx(|s: &mut S, tx| s.press = Some(tx.link_rect())),
            )
    }
    let mut rt = Runtime::new(S::default(), view, (300.0, 80.0), Env::new(true, 1.0));
    rt.frame(0.0);
    rt
}

#[test]
fn a_pressed_link_gives_its_handler_its_box_and_a_plain_press_gives_none() {
    let mut rt = runtime();
    let (x, y) = rt.find("docs").expect("the link is drawn");
    rt.pointer_down(x, y);
    rt.frame(0.016);
    rt.pointer_up(x, y);
    rt.frame(0.032);
    let [left, top, width, height] = rt.state.links.first().copied().flatten().expect("a box");
    assert!(width > 0.0 && height > 0.0);
    assert!((left..left + width).contains(&x) && (top..top + height).contains(&y), "the box holds the press");
    let (bx, by) = rt.find("b").expect("the plain element");
    rt.pointer_down(bx, by);
    rt.frame(0.048);
    rt.pointer_up(bx, by);
    rt.frame(0.064);
    assert_eq!(rt.state.press, Some(None), "only a link handler is told a box");
}

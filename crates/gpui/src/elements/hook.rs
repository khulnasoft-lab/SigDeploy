use std::ops::Range;

use crate::{
    geometry::{rect::RectF, vector::Vector2F},
    json::json,
    presenter::MeasurementContext,
    DebugContext, Element, ElementBox, LayoutContext, PaintContext, SizeConstraint,
};

pub struct Hook {
    child: ElementBox,
    after_layout: Option<Box<dyn FnMut(Vector2F, &mut LayoutContext)>>,
}

impl Hook {
    pub fn new(child: ElementBox) -> Self {
        Self {
            child,
            after_layout: None,
        }
    }

    pub fn on_after_layout(
        mut self,
        f: impl 'static + FnMut(Vector2F, &mut LayoutContext),
    ) -> Self {
        self.after_layout = Some(Box::new(f));
        self
    }
}

impl Element for Hook {
    type LayoutState = ();
    type PaintState = ();

    fn layout(
        &mut self,
        constraint: SizeConstraint,
        cx: &mut LayoutContext,
    ) -> (Vector2F, Self::LayoutState) {
        let size = self.child.layout(constraint, cx);
        if let Some(handler) = self.after_layout.as_mut() {
            handler(size, cx);
        }
        (size, ())
    }

    fn paint(
        &mut self,
        bounds: RectF,
        visible_bounds: RectF,
        _: &mut Self::LayoutState,
        cx: &mut PaintContext,
    ) {
        self.child.paint(bounds.origin(), visible_bounds, cx);
    }

    fn rect_for_text_range(
        &self,
        range_utf16: Range<usize>,
        _: RectF,
        _: RectF,
        _: &Self::LayoutState,
        _: &Self::PaintState,
        cx: &MeasurementContext,
    ) -> Option<RectF> {
        self.child.rect_for_text_range(range_utf16, cx)
    }

    fn debug(
        &self,
        _: RectF,
        _: &Self::LayoutState,
        _: &Self::PaintState,
        cx: &DebugContext,
    ) -> serde_json::Value {
        json!({
            "type": "Hooks",
            "child": self.child.debug(cx),
        })
    }
}

use farever_more_sdk::ui::{
    Alignment, Anchor, CanvasBuilder, Color, Column, ColumnSizing, Frame, FrameBuilder, Image,
    NodeKind, PrimitiveRef, RowKind, RowProgress, Section, Stroke, SurfaceBuilder, SurfaceOptions,
    SurfaceStyle, Text, TextStyle, UiEvent, View,
};

#[test]
fn ui_types_are_available_from_the_ui_module() {
    let _color = Color::rgba8(24, 32, 48, 255);
    let _stroke = Stroke::new(1.0, Color::WHITE);
    let _anchor = Anchor::TopLeft;
    let _alignment = Alignment::Center;
    let _column_sizing = ColumnSizing::Auto;
    let _column = Column::auto();
    let _row_kind = RowKind::Body;
    let _row_progress = RowProgress::new(0.5, Color::WHITE);
    let _surface_style = SurfaceStyle::new(Color::TRANSPARENT);
    let _surface_options = SurfaceOptions::new(Anchor::TopLeft);
    let _view = View::Surface;
    let _event = UiEvent::ConfigMenuShown {
        id: "settings".to_owned(),
    };
    let _frame = Frame::empty();
    let _frame_builder = FrameBuilder::new();
    let _text = Text::new(TextStyle::Body);
    let _section = Section::new("Damage").description("Per-target breakdown");
    let _image = Image {
        id: "icon".to_owned(),
    };

    // Keep callback builder and inspection enum types on the same supported UI
    // surface without constructing values that borrow a frame builder.
    fn assert_exported<T>() {}
    assert_exported::<CanvasBuilder<'_>>();
    assert_exported::<NodeKind>();
    assert_exported::<PrimitiveRef<'_>>();
    assert_exported::<SurfaceBuilder<'_>>();
}

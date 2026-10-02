//! Declarative, renderer-independent UI builders and inspection views.

use crate::__wit::farever::addon::overlay as raw;
pub use crate::assets::{Image, TextStyle};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Color {
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub alpha: f32,
}

impl Color {
    #[must_use]
    pub const fn rgba(red: f32, green: f32, blue: f32, alpha: f32) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    #[must_use]
    pub const fn rgba8(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self::rgba(
            red as f32 / 255.0,
            green as f32 / 255.0,
            blue as f32 / 255.0,
            alpha as f32 / 255.0,
        )
    }

    pub const TRANSPARENT: Self = Self::rgba(0.0, 0.0, 0.0, 0.0);
    pub const WHITE: Self = Self::rgba(1.0, 1.0, 1.0, 1.0);
}

impl From<Color> for raw::Rgba {
    fn from(value: Color) -> Self {
        Self {
            red: value.red,
            green: value.green,
            blue: value.blue,
            alpha: value.alpha,
        }
    }
}

impl From<raw::Rgba> for Color {
    fn from(value: raw::Rgba) -> Self {
        Self::rgba(value.red, value.green, value.blue, value.alpha)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stroke {
    pub width: f32,
    pub color: Color,
}

impl Stroke {
    #[must_use]
    pub const fn new(width: f32, color: Color) -> Self {
        Self { width, color }
    }
}

impl From<Stroke> for raw::Stroke {
    fn from(value: Stroke) -> Self {
        Self {
            width: value.width,
            color: value.color.into(),
        }
    }
}

impl From<raw::Stroke> for Stroke {
    fn from(value: raw::Stroke) -> Self {
        Self::new(value.width, value.color.into())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Anchor {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
    TopCenter,
}

impl From<Anchor> for raw::SurfaceAnchor {
    fn from(value: Anchor) -> Self {
        match value {
            Anchor::TopLeft => Self::TopLeft,
            Anchor::TopRight => Self::TopRight,
            Anchor::BottomLeft => Self::BottomLeft,
            Anchor::BottomRight => Self::BottomRight,
            Anchor::Center => Self::Center,
            Anchor::TopCenter => Self::TopCenter,
        }
    }
}

impl From<raw::SurfaceAnchor> for Anchor {
    fn from(value: raw::SurfaceAnchor) -> Self {
        match value {
            raw::SurfaceAnchor::TopLeft => Self::TopLeft,
            raw::SurfaceAnchor::TopRight => Self::TopRight,
            raw::SurfaceAnchor::BottomLeft => Self::BottomLeft,
            raw::SurfaceAnchor::BottomRight => Self::BottomRight,
            raw::SurfaceAnchor::Center => Self::Center,
            raw::SurfaceAnchor::TopCenter => Self::TopCenter,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Alignment {
    Left,
    Center,
    Right,
}

impl From<Alignment> for raw::HorizontalAlignment {
    fn from(value: Alignment) -> Self {
        match value {
            Alignment::Left => Self::Left,
            Alignment::Center => Self::Center,
            Alignment::Right => Self::Right,
        }
    }
}

impl From<raw::HorizontalAlignment> for Alignment {
    fn from(value: raw::HorizontalAlignment) -> Self {
        match value {
            raw::HorizontalAlignment::Left => Self::Left,
            raw::HorizontalAlignment::Center => Self::Center,
            raw::HorizontalAlignment::Right => Self::Right,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColumnSizing {
    Auto,
    Pixels(f32),
    Flex,
}

pub struct Column {
    sizing: ColumnSizing,
    alignment: Alignment,
    visible_from_width: Option<f32>,
    padding: Option<f32>,
}

impl Column {
    #[must_use]
    pub const fn auto() -> Self {
        Self::new(ColumnSizing::Auto)
    }

    #[must_use]
    pub const fn pixels(width: f32) -> Self {
        Self::new(ColumnSizing::Pixels(width))
    }

    #[must_use]
    pub const fn flex() -> Self {
        Self::new(ColumnSizing::Flex)
    }

    const fn new(sizing: ColumnSizing) -> Self {
        Self {
            sizing,
            alignment: Alignment::Left,
            visible_from_width: None,
            padding: None,
        }
    }

    #[must_use]
    pub const fn align(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    #[must_use]
    pub const fn visible_from(mut self, width: f32) -> Self {
        self.visible_from_width = Some(width);
        self
    }

    #[must_use]
    pub const fn padding(mut self, padding: f32) -> Self {
        self.padding = Some(padding);
        self
    }

    fn into_raw(self) -> raw::TableColumn {
        raw::TableColumn {
            sizing: match self.sizing {
                ColumnSizing::Auto => raw::TableColumnSizing::Auto,
                ColumnSizing::Pixels(width) => raw::TableColumnSizing::Exact(width),
                ColumnSizing::Flex => raw::TableColumnSizing::Remainder,
            },
            alignment: self.alignment.into(),
            visible_from_width: self.visible_from_width,
            content_padding: self.padding,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowKind {
    Header,
    Body,
}

impl From<RowKind> for raw::TableRowKind {
    fn from(value: RowKind) -> Self {
        match value {
            RowKind::Header => Self::Header,
            RowKind::Body => Self::Body,
        }
    }
}

pub struct RowProgress {
    fraction: f32,
    color: Color,
    start_column: Option<u32>,
}

impl RowProgress {
    #[must_use]
    pub const fn new(fraction: f32, color: Color) -> Self {
        Self {
            fraction,
            color,
            start_column: None,
        }
    }

    #[must_use]
    pub const fn starting_at(mut self, column: u32) -> Self {
        self.start_column = Some(column);
        self
    }

    fn into_raw(self) -> raw::TableRowProgress {
        raw::TableRowProgress {
            fraction: self.fraction,
            color: self.color.into(),
            start_column: self.start_column,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceStyle {
    title_bar: bool,
    fill: Color,
    stroke: Option<Stroke>,
    corner_radius: f32,
    padding: f32,
}

impl SurfaceStyle {
    #[must_use]
    pub const fn new(fill: Color) -> Self {
        Self {
            title_bar: false,
            fill,
            stroke: None,
            corner_radius: 0.0,
            padding: 0.0,
        }
    }

    #[must_use]
    pub const fn title_bar(mut self, visible: bool) -> Self {
        self.title_bar = visible;
        self
    }

    #[must_use]
    pub const fn stroke(mut self, stroke: Stroke) -> Self {
        self.stroke = Some(stroke);
        self
    }

    #[must_use]
    pub const fn corner_radius(mut self, radius: f32) -> Self {
        self.corner_radius = radius;
        self
    }

    #[must_use]
    pub const fn padding(mut self, padding: f32) -> Self {
        self.padding = padding;
        self
    }

    fn into_raw(self) -> raw::SurfaceStyle {
        raw::SurfaceStyle {
            title_bar: self.title_bar,
            fill: self.fill.into(),
            stroke: self.stroke.map(Into::into),
            corner_radius: self.corner_radius,
            padding: self.padding,
        }
    }

    #[must_use]
    pub const fn has_title_bar(&self) -> bool {
        self.title_bar
    }

    #[must_use]
    pub const fn content_padding(&self) -> f32 {
        self.padding
    }

    #[must_use]
    pub const fn fill(&self) -> Color {
        self.fill
    }

    #[must_use]
    pub const fn border(&self) -> Option<Stroke> {
        self.stroke
    }
}

pub struct SurfaceOptions {
    anchor: Anchor,
    margin: [f32; 2],
    width: Option<f32>,
    style: Option<SurfaceStyle>,
}

impl SurfaceOptions {
    #[must_use]
    pub const fn new(anchor: Anchor) -> Self {
        Self {
            anchor,
            margin: [0.0, 0.0],
            width: None,
            style: None,
        }
    }

    #[must_use]
    pub const fn margin(mut self, x: f32, y: f32) -> Self {
        self.margin = [x, y];
        self
    }

    #[must_use]
    pub const fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    #[must_use]
    pub const fn style(mut self, style: SurfaceStyle) -> Self {
        self.style = Some(style);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum View {
    Surface,
    ConfigMenu,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiEvent {
    ConfigMenuShown {
        id: String,
    },
    ConfigMenuHidden {
        id: String,
    },
    ButtonPressed {
        view: View,
        view_id: String,
        id: String,
    },
    CheckboxChanged {
        id: String,
        checked: bool,
    },
    /// Pointer press inside a canvas node. `x`/`y` are canvas-local logical
    /// points, which is what the add-on's own drawing is laid out in.
    CanvasPressed {
        view: View,
        view_id: String,
        id: String,
        x: f64,
        y: f64,
    },
    DropdownChanged {
        id: String,
        selected_id: String,
    },
    SliderChanged {
        id: String,
        value: f64,
    },
}

impl UiEvent {
    #[doc(hidden)]
    pub fn from_raw(value: raw::UiEvent) -> Self {
        match value {
            raw::UiEvent::ConfigMenuShown(id) => Self::ConfigMenuShown { id },
            raw::UiEvent::ConfigMenuHidden(id) => Self::ConfigMenuHidden { id },
            raw::UiEvent::ButtonPressed(button) => {
                let (view, view_id) = match button.view {
                    raw::UiView::Surface(id) => (View::Surface, id),
                    raw::UiView::ConfigMenu(id) => (View::ConfigMenu, id),
                };
                Self::ButtonPressed {
                    view,
                    view_id,
                    id: button.node_id,
                }
            }
            raw::UiEvent::CheckboxChanged((id, checked)) => Self::CheckboxChanged { id, checked },
            raw::UiEvent::CanvasPressed(canvas) => {
                let (view, view_id) = match canvas.view {
                    raw::UiView::Surface(id) => (View::Surface, id),
                    raw::UiView::ConfigMenu(id) => (View::ConfigMenu, id),
                };
                Self::CanvasPressed {
                    view,
                    view_id,
                    id: canvas.node_id,
                    x: canvas.position.0,
                    y: canvas.position.1,
                }
            }
            raw::UiEvent::DropdownChanged((id, selected_id)) => {
                Self::DropdownChanged { id, selected_id }
            }
            raw::UiEvent::SliderChanged((id, value)) => Self::SliderChanged { id, value },
        }
    }
}

pub struct Frame {
    raw: raw::UiFrame,
    passive_canvases: Vec<crate::__wit::farever::addon::canvas_options::CanvasRef>,
}

impl Frame {
    #[must_use]
    pub fn empty() -> Self {
        FrameBuilder::new().finish()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.surfaces.is_empty() && self.raw.config_menus.is_empty()
    }

    #[must_use]
    pub fn surface_count(&self) -> usize {
        self.raw.surfaces.len()
    }

    #[must_use]
    pub fn config_menu_count(&self) -> usize {
        self.raw.config_menus.len()
    }

    pub fn surfaces(&self) -> impl Iterator<Item = SurfaceRef<'_>> {
        self.raw.surfaces.iter().map(|raw| SurfaceRef { raw })
    }

    pub fn surface(&self, id: &str) -> Option<SurfaceRef<'_>> {
        self.raw
            .surfaces
            .iter()
            .find(|surface| surface.id == id)
            .map(|raw| SurfaceRef { raw })
    }

    pub fn config_menu(&self, id: &str) -> Option<MenuRef<'_>> {
        self.raw
            .config_menus
            .iter()
            .find(|menu| menu.id == id)
            .map(|raw| MenuRef { raw })
    }

    #[doc(hidden)]
    pub fn into_raw(self) -> raw::UiFrame {
        #[cfg(target_arch = "wasm32")]
        crate::__wit::farever::addon::canvas_options::set_passive_canvases(&self.passive_canvases)
            .expect("host rejected canvas options");
        #[cfg(not(target_arch = "wasm32"))]
        let _ = self.passive_canvases;
        self.raw
    }
}

pub struct FrameBuilder {
    surfaces: Vec<raw::UiSurface>,
    config_menus: Vec<raw::ConfigMenu>,
    passive_canvases: Vec<crate::__wit::farever::addon::canvas_options::CanvasRef>,
}

impl Default for FrameBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            surfaces: Vec::new(),
            config_menus: Vec::new(),
            passive_canvases: Vec::new(),
        }
    }

    pub fn config_menu(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        build: impl FnOnce(&mut SurfaceBuilder<'_>),
    ) {
        let (nodes, canvas, passive) = build_nodes(build);
        assert!(
            passive.is_empty(),
            "passive canvases are only supported in overlay surfaces"
        );
        self.config_menus.push(raw::ConfigMenu {
            id: id.into(),
            title: title.into(),
            nodes,
            canvas,
        });
    }

    pub fn surface(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        options: SurfaceOptions,
        build: impl FnOnce(&mut SurfaceBuilder<'_>),
    ) {
        let (nodes, canvas, passive) = build_nodes(build);
        let id = id.into();
        self.passive_canvases
            .extend(passive.into_iter().map(|node_id| {
                crate::__wit::farever::addon::canvas_options::CanvasRef {
                    surface_id: id.clone(),
                    node_id,
                }
            }));
        self.surfaces.push(raw::UiSurface {
            id,
            title: title.into(),
            anchor: options.anchor.into(),
            margin_x: options.margin[0],
            margin_y: options.margin[1],
            width: options.width,
            style: options.style.map(SurfaceStyle::into_raw),
            nodes,
            canvas,
        });
    }

    #[must_use]
    pub fn finish(self) -> Frame {
        Frame {
            passive_canvases: self.passive_canvases,
            raw: raw::UiFrame {
                surfaces: self.surfaces,
                config_menus: self.config_menus,
            },
        }
    }
}

fn build_nodes(
    build: impl FnOnce(&mut SurfaceBuilder<'_>),
) -> (Vec<raw::UiNode>, Vec<raw::CanvasCommand>, Vec<String>) {
    let mut nodes = Vec::new();
    let mut canvas = Vec::new();
    let mut passive = Vec::new();
    build(&mut SurfaceBuilder {
        nodes: &mut nodes,
        canvas: &mut canvas,
        passive: &mut passive,
        parent: None,
    });
    (nodes, canvas, passive)
}

/// One titled section: the host draws the header with the same styling it uses
/// for its own page headers and lays the nodes built inside it out underneath.
/// Prefer this over a group plus a heading text.
pub struct Section {
    title: String,
    description: Option<String>,
}

impl Section {
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            description: None,
        }
    }

    /// Adds a muted line drawn under the title.
    #[must_use]
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }
}

pub struct SurfaceBuilder<'a> {
    nodes: &'a mut Vec<raw::UiNode>,
    canvas: &'a mut Vec<raw::CanvasCommand>,
    passive: &'a mut Vec<String>,
    parent: Option<String>,
}

impl SurfaceBuilder<'_> {
    pub fn vertical(
        &mut self,
        id: impl Into<String>,
        spacing: Option<f32>,
        build: impl FnOnce(&mut Self),
    ) {
        self.container(
            id,
            raw::LayoutDirection::Vertical,
            raw::ContainerStyle::Plain,
            spacing,
            None,
            build,
        );
    }

    pub fn row(
        &mut self,
        id: impl Into<String>,
        spacing: Option<f32>,
        build: impl FnOnce(&mut Self),
    ) {
        self.container(
            id,
            raw::LayoutDirection::Horizontal,
            raw::ContainerStyle::Plain,
            spacing,
            None,
            build,
        );
    }

    pub fn group(&mut self, id: impl Into<String>, build: impl FnOnce(&mut Self)) {
        self.container(
            id,
            raw::LayoutDirection::Vertical,
            raw::ContainerStyle::Group,
            None,
            None,
            build,
        );
    }

    /// A titled section: the host draws the header and lays the children out
    /// underneath it. Sections may own ordinary widget children.
    pub fn section(
        &mut self,
        id: impl Into<String>,
        section: Section,
        build: impl FnOnce(&mut Self),
    ) {
        let id = id.into();
        self.node(
            id.clone(),
            raw::Widget::Section(raw::SectionWidget {
                title: section.title,
                description: section.description,
            }),
        );
        self.with_parent(id, build);
    }

    pub fn scroll(
        &mut self,
        id: impl Into<String>,
        max_height: f32,
        build: impl FnOnce(&mut Self),
    ) {
        self.container(
            id,
            raw::LayoutDirection::Vertical,
            raw::ContainerStyle::Scroll,
            None,
            Some(max_height),
            build,
        );
    }

    pub fn table(
        &mut self,
        id: impl Into<String>,
        columns: Vec<Column>,
        striped: bool,
        max_body_height: Option<f32>,
        build: impl FnOnce(&mut Self),
    ) {
        let id = id.into();
        self.node(
            id.clone(),
            raw::Widget::Table(raw::TableWidget {
                columns: columns.into_iter().map(Column::into_raw).collect(),
                striped,
                max_body_height,
            }),
        );
        self.with_parent(id, build);
    }

    pub fn table_row(
        &mut self,
        id: impl Into<String>,
        kind: RowKind,
        height: f32,
        build: impl FnOnce(&mut Self),
    ) {
        self.styled_table_row(id, kind, height, None, None, build);
    }

    pub fn styled_table_row(
        &mut self,
        id: impl Into<String>,
        kind: RowKind,
        height: f32,
        background: Option<Color>,
        progress: Option<RowProgress>,
        build: impl FnOnce(&mut Self),
    ) {
        let id = id.into();
        self.node(
            id.clone(),
            raw::Widget::TableRow(raw::TableRowWidget {
                kind: kind.into(),
                height,
                background: background.map(Into::into),
                progress: progress.map(RowProgress::into_raw),
            }),
        );
        self.with_parent(id, build);
    }

    pub fn table_cell(
        &mut self,
        id: impl Into<String>,
        column: u32,
        build: impl FnOnce(&mut Self),
    ) {
        let id = id.into();
        self.node(
            id.clone(),
            raw::Widget::TableCell(raw::TableCellWidget { column }),
        );
        self.with_parent(id, build);
    }

    pub fn heading(&mut self, id: impl Into<String>, text: impl Into<String>) {
        self.text(id, text, Text::new(TextStyle::Heading).no_wrap());
    }

    pub fn label(&mut self, id: impl Into<String>, text: impl Into<String>) {
        self.text(id, text, Text::new(TextStyle::Body));
    }

    pub fn small(&mut self, id: impl Into<String>, text: impl Into<String>) {
        self.text(id, text, Text::new(TextStyle::Small));
    }

    pub fn strong(&mut self, id: impl Into<String>, text: impl Into<String>) {
        self.text(id, text, Text::new(TextStyle::Strong));
    }

    pub fn monospace(&mut self, id: impl Into<String>, text: impl Into<String>) {
        self.text(id, text, Text::new(TextStyle::Monospace));
    }

    pub fn text(&mut self, id: impl Into<String>, text: impl Into<String>, options: Text) {
        self.node(
            id,
            raw::Widget::Text(raw::TextWidget {
                text: text.into(),
                style: options.style.into(),
                color: options.color.map(Into::into),
                outline: options.outline.map(Into::into),
                wrap: options.wrap,
            }),
        );
    }

    pub fn image(
        &mut self,
        id: impl Into<String>,
        source: Image,
        size: [f32; 2],
        tint: Option<Color>,
    ) {
        self.node(
            id,
            raw::Widget::Image(raw::ImageWidget {
                source: source.into(),
                size: raw::Size {
                    width: size[0],
                    height: size[1],
                },
                tint: tint.map(Into::into),
            }),
        );
    }

    pub fn progress(
        &mut self,
        id: impl Into<String>,
        fraction: f32,
        label: impl Into<String>,
        color: Option<Color>,
    ) {
        self.node(
            id,
            raw::Widget::Progress(raw::ProgressWidget {
                fraction,
                label: Some(label.into()),
                color: color.map(Into::into),
            }),
        );
    }

    pub fn button(&mut self, id: impl Into<String>, label: impl Into<String>, enabled: bool) {
        self.node(
            id,
            raw::Widget::Button(raw::ButtonWidget {
                label: label.into(),
                enabled,
            }),
        );
    }

    pub fn checkbox(
        &mut self,
        id: impl Into<String>,
        label: impl Into<String>,
        checked: bool,
        enabled: bool,
    ) {
        self.node(
            id,
            raw::Widget::Checkbox(raw::CheckboxWidget {
                label: label.into(),
                checked,
                enabled,
            }),
        );
    }

    /// Adds a controlled single-choice setting to a config menu.
    ///
    /// The selected ID is returned in [`UiEvent::DropdownChanged`] when the
    /// user chooses a different option. Option IDs are add-on-owned stable
    /// values; labels are presentation text.
    pub fn dropdown(
        &mut self,
        id: impl Into<String>,
        label: impl Into<String>,
        selected_id: impl Into<String>,
        options: impl IntoIterator<Item = (String, String)>,
        enabled: bool,
    ) {
        self.node(
            id,
            raw::Widget::Dropdown(raw::DropdownWidget {
                label: label.into(),
                selected_id: selected_id.into(),
                options: options
                    .into_iter()
                    .map(|(id, label)| raw::DropdownOption { id, label })
                    .collect(),
                enabled,
            }),
        );
    }

    pub fn separator(&mut self, id: impl Into<String>) {
        self.node(id, raw::Widget::Separator);
    }

    pub fn spacer(&mut self, id: impl Into<String>, size: f32) {
        self.node(id, raw::Widget::Spacer(raw::SpacerWidget { size }));
    }

    pub fn canvas(
        &mut self,
        id: impl Into<String>,
        size: [f32; 2],
        build: impl FnOnce(&mut CanvasBuilder<'_>),
    ) {
        self.canvas_node(id.into(), size, false, build);
    }

    /// Paint a canvas without intercepting native mouse input. When it is the
    /// only node in a surface, placement is exact and has no window decoration.
    pub fn passive_canvas(
        &mut self,
        id: impl Into<String>,
        size: [f32; 2],
        build: impl FnOnce(&mut CanvasBuilder<'_>),
    ) {
        self.canvas_node(id.into(), size, true, build);
    }

    fn canvas_node(
        &mut self,
        id: String,
        size: [f32; 2],
        passive: bool,
        build: impl FnOnce(&mut CanvasBuilder<'_>),
    ) {
        let widget = raw::CanvasWidget {
            size: raw::Size {
                width: size[0],
                height: size[1],
            },
        };
        self.node(id.clone(), raw::Widget::Canvas(widget));
        if passive {
            self.passive.push(id.clone());
        }
        build(&mut CanvasBuilder {
            id,
            commands: self.canvas,
        });
    }

    fn container(
        &mut self,
        id: impl Into<String>,
        direction: raw::LayoutDirection,
        style: raw::ContainerStyle,
        spacing: Option<f32>,
        max_height: Option<f32>,
        build: impl FnOnce(&mut Self),
    ) {
        let id = id.into();
        self.node(
            id.clone(),
            raw::Widget::Container(raw::ContainerWidget {
                direction,
                style,
                spacing,
                max_height,
            }),
        );
        self.with_parent(id, build);
    }

    fn with_parent(&mut self, id: String, build: impl FnOnce(&mut Self)) {
        let previous = self.parent.replace(id);
        build(self);
        self.parent = previous;
    }

    fn node(&mut self, id: impl Into<String>, widget: raw::Widget) {
        self.nodes.push(raw::UiNode {
            id: id.into(),
            parent: self.parent.clone(),
            widget,
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Text {
    style: TextStyle,
    color: Option<Color>,
    outline: Option<Stroke>,
    wrap: bool,
}

impl Text {
    #[must_use]
    pub const fn new(style: TextStyle) -> Self {
        Self {
            style,
            color: None,
            outline: None,
            wrap: true,
        }
    }

    #[must_use]
    pub const fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    #[must_use]
    pub const fn outline(mut self, outline: Stroke) -> Self {
        self.outline = Some(outline);
        self
    }

    #[must_use]
    pub const fn no_wrap(mut self) -> Self {
        self.wrap = false;
        self
    }
}

pub struct CanvasBuilder<'a> {
    id: String,
    commands: &'a mut Vec<raw::CanvasCommand>,
}

impl CanvasBuilder<'_> {
    pub fn line(&mut self, start: [f32; 2], end: [f32; 2], stroke: Stroke) {
        self.push(raw::CanvasPrimitive::Line(raw::LinePrimitive {
            start: point(start),
            end: point(end),
            stroke: stroke.into(),
        }));
    }

    pub fn rect(
        &mut self,
        min: [f32; 2],
        max: [f32; 2],
        corner_radius: f32,
        fill: Option<Color>,
        stroke: Option<Stroke>,
    ) {
        self.push(raw::CanvasPrimitive::Rect(raw::RectPrimitive {
            min: point(min),
            max: point(max),
            corner_radius,
            fill: fill.map(Into::into),
            stroke: stroke.map(Into::into),
        }));
    }

    pub fn circle(
        &mut self,
        center: [f32; 2],
        radius: f32,
        fill: Option<Color>,
        stroke: Option<Stroke>,
    ) {
        self.push(raw::CanvasPrimitive::Circle(raw::CirclePrimitive {
            center: point(center),
            radius,
            fill: fill.map(Into::into),
            stroke: stroke.map(Into::into),
        }));
    }

    pub fn path(
        &mut self,
        points: impl IntoIterator<Item = [f32; 2]>,
        closed: bool,
        fill: Option<Color>,
        stroke: Option<Stroke>,
    ) {
        self.push(raw::CanvasPrimitive::Path(raw::PathPrimitive {
            points: points.into_iter().map(point).collect(),
            closed,
            fill: fill.map(Into::into),
            stroke: stroke.map(Into::into),
        }));
    }

    pub fn text(&mut self, position: [f32; 2], text: impl Into<String>, color: Color, size: f32) {
        self.push(raw::CanvasPrimitive::Text(raw::CanvasTextPrimitive {
            position: point(position),
            text: text.into(),
            color: color.into(),
            size,
        }));
    }

    /// Draws a host-owned image using normalized source UVs. The destination
    /// is rotated around its center and may be clipped to a rounded rectangle
    /// or circle by the host renderer.
    pub fn image(
        &mut self,
        source: Image,
        destination_min: [f32; 2],
        destination_max: [f32; 2],
        uv_min: [f32; 2],
        uv_max: [f32; 2],
        rotation_radians: f32,
        tint: Option<Color>,
        corner_radius: f32,
    ) {
        self.push(raw::CanvasPrimitive::Image(raw::CanvasImagePrimitive {
            source: source.into(),
            destination_min: point(destination_min),
            destination_max: point(destination_max),
            uv_min: point(uv_min),
            uv_max: point(uv_max),
            rotation_radians,
            tint: tint.map(Into::into),
            corner_radius,
        }));
    }

    fn push(&mut self, primitive: raw::CanvasPrimitive) {
        self.commands.push(raw::CanvasCommand {
            canvas_id: self.id.clone(),
            primitive,
        });
    }
}

fn point([x, y]: [f32; 2]) -> raw::Point {
    raw::Point { x, y }
}

pub struct SurfaceRef<'a> {
    raw: &'a raw::UiSurface,
}

impl<'a> SurfaceRef<'a> {
    pub fn id(&self) -> &'a str {
        &self.raw.id
    }
    pub fn anchor(&self) -> Anchor {
        self.raw.anchor.into()
    }
    pub fn margin(&self) -> [f32; 2] {
        [self.raw.margin_x, self.raw.margin_y]
    }
    pub fn width(&self) -> Option<f32> {
        self.raw.width
    }
    pub fn style(&self) -> Option<SurfaceStyle> {
        self.raw.style.as_ref().map(|style| SurfaceStyle {
            title_bar: style.title_bar,
            fill: style.fill.into(),
            stroke: style.stroke.map(Into::into),
            corner_radius: style.corner_radius,
            padding: style.padding,
        })
    }
    pub fn nodes(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        self.raw.nodes.iter().map(|raw| NodeRef { raw })
    }
    pub fn node(&self, id: &str) -> Option<NodeRef<'a>> {
        self.raw
            .nodes
            .iter()
            .find(|node| node.id == id)
            .map(|raw| NodeRef { raw })
    }
    pub fn canvas(&self) -> impl Iterator<Item = CanvasCommandRef<'a>> + 'a {
        self.raw.canvas.iter().map(|raw| CanvasCommandRef { raw })
    }
}

pub struct MenuRef<'a> {
    raw: &'a raw::ConfigMenu,
}
impl<'a> MenuRef<'a> {
    pub fn id(&self) -> &'a str {
        &self.raw.id
    }
    pub fn nodes(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        self.raw.nodes.iter().map(|raw| NodeRef { raw })
    }
    pub fn node(&self, id: &str) -> Option<NodeRef<'a>> {
        self.raw
            .nodes
            .iter()
            .find(|node| node.id == id)
            .map(|raw| NodeRef { raw })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Container,
    Section,
    Text,
    Image,
    Button,
    Checkbox,
    Dropdown,
    Slider,
    Progress,
    Separator,
    Spacer,
    Canvas,
    Table,
    TableRow,
    TableCell,
}

pub struct NodeRef<'a> {
    raw: &'a raw::UiNode,
}
impl<'a> NodeRef<'a> {
    pub fn id(&self) -> &'a str {
        &self.raw.id
    }
    pub fn parent(&self) -> Option<&'a str> {
        self.raw.parent.as_deref()
    }
    pub fn kind(&self) -> NodeKind {
        match self.raw.widget {
            raw::Widget::Container(_) => NodeKind::Container,
            raw::Widget::Section(_) => NodeKind::Section,
            raw::Widget::Text(_) => NodeKind::Text,
            raw::Widget::Image(_) => NodeKind::Image,
            raw::Widget::Button(_) => NodeKind::Button,
            raw::Widget::Checkbox(_) => NodeKind::Checkbox,
            raw::Widget::Dropdown(_) => NodeKind::Dropdown,
            raw::Widget::Slider(_) => NodeKind::Slider,
            raw::Widget::Progress(_) => NodeKind::Progress,
            raw::Widget::Separator => NodeKind::Separator,
            raw::Widget::Spacer(_) => NodeKind::Spacer,
            raw::Widget::Canvas(_) => NodeKind::Canvas,
            raw::Widget::Table(_) => NodeKind::Table,
            raw::Widget::TableRow(_) => NodeKind::TableRow,
            raw::Widget::TableCell(_) => NodeKind::TableCell,
        }
    }
    pub fn text(&self) -> Option<&'a str> {
        match &self.raw.widget {
            raw::Widget::Text(text) => Some(&text.text),
            _ => None,
        }
    }
    pub fn text_color(&self) -> Option<Color> {
        match &self.raw.widget {
            raw::Widget::Text(text) => text.color.map(Into::into),
            _ => None,
        }
    }
    pub fn text_outline(&self) -> Option<Stroke> {
        match &self.raw.widget {
            raw::Widget::Text(text) => text.outline.map(Into::into),
            _ => None,
        }
    }
    pub fn checkbox_checked(&self) -> Option<bool> {
        match &self.raw.widget {
            raw::Widget::Checkbox(value) => Some(value.checked),
            _ => None,
        }
    }
    pub fn checkbox(&self) -> Option<(&'a str, bool, bool)> {
        match &self.raw.widget {
            raw::Widget::Checkbox(value) => Some((&value.label, value.checked, value.enabled)),
            _ => None,
        }
    }
    pub fn image(&self) -> Option<(Image, [f32; 2])> {
        match &self.raw.widget {
            raw::Widget::Image(image) => Some((
                image.source.clone().into(),
                [image.size.width, image.size.height],
            )),
            _ => None,
        }
    }
    pub fn container_spacing(&self) -> Option<f32> {
        match &self.raw.widget {
            raw::Widget::Container(container) => container.spacing,
            _ => None,
        }
    }
    /// Header text and optional muted description of a section node.
    pub fn section(&self) -> Option<(&'a str, Option<&'a str>)> {
        match &self.raw.widget {
            raw::Widget::Section(section) => Some((&section.title, section.description.as_deref())),
            _ => None,
        }
    }
    pub fn columns(&self) -> Option<Vec<ColumnInfo>> {
        match &self.raw.widget {
            raw::Widget::Table(table) => Some(table.columns.iter().map(ColumnInfo::from).collect()),
            _ => None,
        }
    }
    pub fn row_progress(&self) -> Option<RowProgressInfo> {
        match &self.raw.widget {
            raw::Widget::TableRow(row) => row.progress.as_ref().map(RowProgressInfo::from),
            _ => None,
        }
    }
    pub fn row_height(&self) -> Option<f32> {
        match &self.raw.widget {
            raw::Widget::TableRow(row) => Some(row.height),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnInfo {
    pub sizing: ColumnSizing,
    pub alignment: Alignment,
    pub visible_from_width: Option<f32>,
    pub padding: Option<f32>,
}
impl From<&raw::TableColumn> for ColumnInfo {
    fn from(value: &raw::TableColumn) -> Self {
        Self {
            sizing: match value.sizing {
                raw::TableColumnSizing::Auto => ColumnSizing::Auto,
                raw::TableColumnSizing::Exact(width) => ColumnSizing::Pixels(width),
                raw::TableColumnSizing::Remainder => ColumnSizing::Flex,
            },
            alignment: value.alignment.into(),
            visible_from_width: value.visible_from_width,
            padding: value.content_padding,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowProgressInfo {
    pub fraction: f32,
    pub color: Color,
    pub start_column: Option<u32>,
}
impl From<&raw::TableRowProgress> for RowProgressInfo {
    fn from(value: &raw::TableRowProgress) -> Self {
        Self {
            fraction: value.fraction,
            color: value.color.into(),
            start_column: value.start_column,
        }
    }
}

pub struct CanvasCommandRef<'a> {
    raw: &'a raw::CanvasCommand,
}
impl<'a> CanvasCommandRef<'a> {
    pub fn canvas_id(&self) -> &'a str {
        &self.raw.canvas_id
    }
    pub fn primitive(&self) -> PrimitiveRef<'_> {
        match &self.raw.primitive {
            raw::CanvasPrimitive::Line(line) => PrimitiveRef::Line(LineRef { raw: line }),
            raw::CanvasPrimitive::Rect(_) => PrimitiveRef::Rect,
            raw::CanvasPrimitive::Circle(_) => PrimitiveRef::Circle,
            raw::CanvasPrimitive::Path(path) => PrimitiveRef::Path(PathRef { raw: path }),
            raw::CanvasPrimitive::Text(_) => PrimitiveRef::Text,
            raw::CanvasPrimitive::Image(_) => PrimitiveRef::Image,
        }
    }
}

pub enum PrimitiveRef<'a> {
    Line(LineRef<'a>),
    Rect,
    Circle,
    Path(PathRef<'a>),
    Text,
    Image,
}
pub struct LineRef<'a> {
    raw: &'a raw::LinePrimitive,
}
impl LineRef<'_> {
    pub fn start(&self) -> [f32; 2] {
        [self.raw.start.x, self.raw.start.y]
    }
    pub fn end(&self) -> [f32; 2] {
        [self.raw.end.x, self.raw.end.y]
    }
}
pub struct PathRef<'a> {
    raw: &'a raw::PathPrimitive,
}
impl PathRef<'_> {
    pub fn points(&self) -> impl Iterator<Item = [f32; 2]> + '_ {
        self.raw.points.iter().map(|point| [point.x, point.y])
    }
    pub fn fill(&self) -> Option<Color> {
        self.raw.fill.map(Into::into)
    }
    pub fn stroke(&self) -> Option<Stroke> {
        self.raw.stroke.map(Into::into)
    }
    pub fn closed(&self) -> bool {
        self.raw.closed
    }
}

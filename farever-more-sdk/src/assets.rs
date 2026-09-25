//! Immutable fonts and images registered with, or supplied by, the host.

use crate::__wit::farever::addon::assets as raw;
use crate::SdkResult;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Image {
    pub id: String,
}

impl From<raw::ImageRef> for Image {
    fn from(value: raw::ImageRef) -> Self {
        Self { id: value.id }
    }
}

impl From<Image> for raw::ImageRef {
    fn from(value: Image) -> Self {
        Self { id: value.id }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextStyle {
    Body,
    Small,
    Strong,
    Heading,
    Monospace,
}

impl From<TextStyle> for raw::TextStyle {
    fn from(value: TextStyle) -> Self {
        match value {
            TextStyle::Body => Self::Body,
            TextStyle::Small => Self::Small,
            TextStyle::Strong => Self::Strong,
            TextStyle::Heading => Self::Heading,
            TextStyle::Monospace => Self::Monospace,
        }
    }
}

/// Asset registration available during activation.
pub struct Assets {
    _private: (),
}

impl Assets {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    pub fn register_font(&self, id: &str, styles: &[TextStyle], bytes: &[u8]) -> SdkResult<()> {
        let styles = styles.iter().copied().map(Into::into).collect::<Vec<_>>();
        raw::register_font(id, &styles, bytes)
    }

    /// Registers one PNG embedded in the add-on and returns its opaque image reference.
    ///
    /// Registration is accepted only while the add-on is activating. The host
    /// validates, decodes, namespaces, and retains the immutable image.
    pub fn register_image(&self, id: &str, png: &[u8]) -> SdkResult<Image> {
        raw::register_image(id, png).map(Into::into)
    }
}

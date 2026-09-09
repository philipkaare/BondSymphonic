//! Style ids the highlighter emits and the two palettes that colour them.

/// The styles a highlighted span can carry. The discriminant doubles as the
/// index into [`STYLE_NAMES`] and into a [`Theme`]'s palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StyleId {
    Keyword,
    String,
    Comment,
    Function,
    Type,
    Number,
    Constant,
    Operator,
    Punctuation,
    Attribute,
    Tag,
    Property,
}

/// Capture names handed to `HighlightConfiguration::configure`, in [`StyleId`]
/// order. tree-sitter matches dotted capture names by prefix, so
/// `keyword.control` resolves to `keyword`.
pub const STYLE_NAMES: [&str; 12] = [
    "keyword",
    "string",
    "comment",
    "function",
    "type",
    "number",
    "constant",
    "operator",
    "punctuation",
    "attribute",
    "tag",
    "property",
];

impl StyleId {
    /// The style at `i` in [`STYLE_NAMES`] order, or `None` when out of range.
    pub fn from_index(i: usize) -> Option<StyleId> {
        const ALL: [StyleId; 12] = [
            StyleId::Keyword,
            StyleId::String,
            StyleId::Comment,
            StyleId::Function,
            StyleId::Type,
            StyleId::Number,
            StyleId::Constant,
            StyleId::Operator,
            StyleId::Punctuation,
            StyleId::Attribute,
            StyleId::Tag,
            StyleId::Property,
        ];
        ALL.get(i).copied()
    }
}

/// How one [`StyleId`] is painted: an `#rrggbb` foreground plus two flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: &'static str,
    pub bold: bool,
    pub italic: bool,
}

/// A full palette: one [`Style`] per [`StyleId`].
pub struct Theme {
    styles: [Style; 12],
}

const fn s(fg: &'static str, bold: bool, italic: bool) -> Style {
    Style { fg, bold, italic }
}

static LIGHT: Theme = Theme {
    styles: [
        s("#a626a4", true, false),  // keyword
        s("#50a14f", false, false), // string
        s("#a0a1a7", false, true),  // comment
        s("#4078f2", false, false), // function
        s("#c18401", false, false), // type
        s("#986801", false, false), // number
        s("#986801", false, false), // constant
        s("#0184bc", false, false), // operator
        s("#383a42", false, false), // punctuation
        s("#e45649", false, false), // attribute
        s("#e45649", false, false), // tag
        s("#e45649", false, false), // property
    ],
};

static DARK: Theme = Theme {
    styles: [
        s("#c678dd", true, false),  // keyword
        s("#98c379", false, false), // string
        s("#5c6370", false, true),  // comment
        s("#61afef", false, false), // function
        s("#e5c07b", false, false), // type
        s("#d19a66", false, false), // number
        s("#d19a66", false, false), // constant
        s("#56b6c2", false, false), // operator
        s("#abb2bf", false, false), // punctuation
        s("#e06c75", false, false), // attribute
        s("#e06c75", false, false), // tag
        s("#e06c75", false, false), // property
    ],
};

impl Theme {
    pub fn light() -> &'static Theme {
        &LIGHT
    }

    pub fn dark() -> &'static Theme {
        &DARK
    }

    pub fn for_dark(dark: bool) -> &'static Theme {
        if dark {
            &DARK
        } else {
            &LIGHT
        }
    }

    pub fn style(&self, id: StyleId) -> Style {
        self.styles[id as usize]
    }
}

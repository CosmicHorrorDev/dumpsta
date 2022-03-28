use std::{
    iter,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use colored::{Color, Colorize};

// Like `vec![]`, but the values can be heterogeneous as long as they can be used in `Disp::from`
macro_rules! disps {
    [$($disp:expr),* $(,)?] => {
        {
            let mut v = Vec::new();
            $(
                v.push(crate::dialog::Disp::from($disp));
            )*

            v
        }
    }
}

pub(crate) use disps;

enum DispType {
    Regular,
    Debug,
}

// TODO: should just store a reference
pub enum Disp {
    Usize(usize),
    Str(String),
    Path(PathBuf),
    Error(anyhow::Error),
}

impl Disp {
    fn fmt(&self, disp_type: &DispType, force_color: &Option<Color>) -> String {
        let s = match disp_type {
            DispType::Regular => match self {
                Self::Usize(val) => val.to_string(),
                Self::Str(val) => val.to_owned(),
                Self::Path(val) => val.to_string_lossy().into_owned(),
                Self::Error(val) => val.to_string(),
            },
            DispType::Debug => match self {
                Self::Usize(val) => format!("{:?}", val),
                Self::Str(val) => format!("{:?}", val),
                Self::Path(val) => format!("{:?}", val),
                Self::Error(val) => format!("{:?}", val),
            },
        };

        let colored_str = match force_color {
            Some(color) => s.color(*color),
            None => match self {
                Self::Usize(_) => s.blue(),
                Self::Str(_) | Self::Path(_) => s.cyan(),
                Self::Error(_) => s.red(),
            },
        };

        format!("{}", colored_str)
    }
}

macro_rules! disp_from {
    ($ty:ty, $variant:expr) => {
        impl From<$ty> for Disp {
            fn from(val: $ty) -> Self {
                $variant(val)
            }
        }
    };
}

disp_from!(usize, Disp::Usize);
disp_from!(String, Disp::Str);
disp_from!(PathBuf, Disp::Path);
disp_from!(anyhow::Error, Disp::Error);

impl From<&str> for Disp {
    fn from(s: &str) -> Self {
        Self::Str(s.to_owned())
    }
}

impl From<&String> for Disp {
    fn from(s: &String) -> Self {
        Self::Str(s.to_owned())
    }
}

impl From<&Path> for Disp {
    fn from(path: &Path) -> Self {
        Self::Path(path.to_owned())
    }
}

#[derive(Clone, Copy, Debug)]
enum Level {
    Info,
    Warn,
    Error,
}

impl From<Level> for Color {
    fn from(level: Level) -> Color {
        match level {
            Level::Info => Color::Blue,
            Level::Warn => Color::Magenta,
            Level::Error => Color::Red,
        }
    }
}

// TODO: take a &str
enum Segment {
    Text(String),
    Marker((DispType, Option<Color>)),
}

impl Segment {
    fn text(s: &str) -> Self {
        Self::Text(s.to_owned())
    }
}

// TODO: this doesn't allow for having { or } in the string
struct FmtStr {
    segments: Vec<Segment>,
}

impl FmtStr {
    fn try_new(s: &str) -> Option<Self> {
        let mut segments = Vec::new();
        let mut splits = s.split('{');

        let first_text = splits.next()?;
        segments.push(Segment::text(first_text));

        // From this point on each split should start with whatever was within {} and then follow
        // with some text
        for split in splits {
            let (within_marker, text) = split.split_once('}')?;
            let disp_type = match within_marker {
                "" => DispType::Regular,
                ":?" => DispType::Debug,
                _ => {
                    return None;
                }
            };
            segments.push(Segment::Marker((disp_type, None)));
            segments.push(Segment::text(text));
        }

        Some(Self { segments })
    }

    fn try_fmt(&self, disps: &[Disp]) -> Option<String> {
        let segments = self.segments.iter();
        let mut disps = disps.iter();

        let mut s = String::new();
        for segment in segments {
            match segment {
                Segment::Text(text) => s.push_str(&text),
                Segment::Marker((disp_type, force_color)) => {
                    let disp = disps.next()?;
                    s.push_str(&disp.fmt(disp_type, force_color));
                }
            }
        }

        if disps.next().is_some() {
            return None;
        }

        Some(s)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Dialog {
    indent: NonZeroUsize,
}

// TODO: make these operations fallible
impl Dialog {
    pub fn raw_with_indent(indent: NonZeroUsize) -> Self {
        Self { indent }
    }

    pub fn new(msg: &str) -> Self {
        Self::new_with(msg, &[])
    }

    pub fn new_with(msg: &str, disps: impl AsRef<[Disp]>) -> Self {
        let fmt_str = FmtStr::try_new(msg).unwrap();
        let disps = disps.as_ref();

        let msg = fmt_str.try_fmt(disps).unwrap();
        eprintln!("{}", msg.bold());

        Self {
            indent: NonZeroUsize::new(1).unwrap(),
        }
    }

    pub fn info(&self, msg: &str) -> Self {
        self.info_with(msg, &[])
    }

    pub fn info_with(&self, msg: &str, disps: impl AsRef<[Disp]>) -> Self {
        self.msg_with(Level::Info.into(), msg, disps)
    }

    #[allow(dead_code)]
    pub fn info_str(&self, msg: &str) -> (Self, String) {
        self.info_str_with(msg, &[])
    }

    #[allow(dead_code)]
    pub fn info_str_with(&self, msg: &str, disps: impl AsRef<[Disp]>) -> (Self, String) {
        self.msg_str_with(Level::Info.into(), msg, disps)
    }

    #[allow(dead_code)]
    pub fn warn(&self, msg: &str) -> Self {
        self.warn_with(msg, &[])
    }

    pub fn warn_with(&self, msg: &str, disps: impl AsRef<[Disp]>) -> Self {
        self.msg_with(Level::Warn.into(), msg, disps)
    }

    #[allow(dead_code)]
    pub fn warn_str(&self, msg: &str) -> (Self, String) {
        self.warn_str_with(msg, &[])
    }

    #[allow(dead_code)]
    pub fn warn_str_with(&self, msg: &str, disps: impl AsRef<[Disp]>) -> (Self, String) {
        self.msg_str_with(Level::Warn.into(), msg, disps)
    }

    #[allow(dead_code)]
    pub fn error(&self, msg: &str) -> Self {
        self.error_with(msg, &[])
    }

    #[allow(dead_code)]
    pub fn error_with(&self, msg: &str, disps: impl AsRef<[Disp]>) -> Self {
        self.msg_with(Level::Error.into(), msg, disps)
    }

    #[allow(dead_code)]
    pub fn error_str(&self, msg: &str) -> (Self, String) {
        self.error_str_with(msg, &[])
    }

    #[allow(dead_code)]
    pub fn error_str_with(&self, msg: &str, disps: impl AsRef<[Disp]>) -> (Self, String) {
        self.msg_str_with(Level::Error.into(), msg, disps)
    }

    #[allow(dead_code)]
    pub fn msg(&self, color: Color, msg: &str) -> Self {
        self.msg_with(color, msg, &[])
    }

    #[allow(dead_code)]
    pub fn msg_with(&self, color: Color, msg: &str, disps: impl AsRef<[Disp]>) -> Self {
        let (sub_dialog, msg) = self.msg_str_with(color, msg, disps);
        eprintln!("{msg}");
        sub_dialog
    }

    #[allow(dead_code)]
    pub fn msg_str(&self, color: Color, msg: &str) -> (Self, String) {
        self.msg_str_with(color, msg, &[])
    }

    pub fn msg_str_with(
        &self,
        color: Color,
        msg: &str,
        disps: impl AsRef<[Disp]>,
    ) -> (Self, String) {
        let arrow = "->".color(color).bold();

        let indent_str: String = iter::repeat("  ").take(self.indent.get() - 1).collect();

        let fmt_str = FmtStr::try_new(msg).unwrap();
        let msg = fmt_str.try_fmt(disps.as_ref()).unwrap();
        let pretty_msg = format!("{}{} {}", indent_str, arrow, msg);

        let indent = self.indent.get().saturating_add(1);
        let sub_dialog = Self {
            indent: NonZeroUsize::new(indent).unwrap(),
        };
        (sub_dialog, pretty_msg)
    }
}

use core::fmt::Display;
#[cfg(feature = "std")]
#[doc(inline)]
pub use standard::*;
#[cfg(feature = "std")]
mod standard {
    use std::path::{self, Path, PathBuf};

    pub trait PathAsDisplay {
        fn as_display(&self) -> path::Display<'_>;
    }

    impl PathAsDisplay for Path {
        fn as_display(&self) -> path::Display<'_> {
            self.display()
        }
    }

    impl PathAsDisplay for PathBuf {
        fn as_display(&self) -> path::Display<'_> {
            self.display()
        }
    }
}

pub trait DisplayAsDisplay {
    fn as_display(&self) -> Self;
}

impl<T: Display> DisplayAsDisplay for &T {
    fn as_display(&self) -> Self {
        self
    }
}

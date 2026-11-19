//! The part of iris's CPU core the display transport names.

pub mod mips_core {
    /// A reader of a CPU's cycle counter; the engine has none.
    #[derive(Clone, Copy, Default)]
    pub struct CyclesPtr;

    impl CyclesPtr {
        pub fn dangling() -> Self {
            CyclesPtr
        }
    }
}

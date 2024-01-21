use crate::cpu::percpu::this_cpu_unsafe;
use crate::sev::ghcb::GHCB;

use core::ops::{Deref, DerefMut};

pub enum GHCBConsumer {
    Console,
}

pub struct GHCBNestingRef {
    ghcb: &'static mut GHCB,
    consumer: GHCBConsumer,
}

impl Deref for GHCBNestingRef {
    type Target = GHCB;
    fn deref(&self) -> &GHCB {
        self.ghcb
    }
}

impl DerefMut for GHCBNestingRef {
    fn deref_mut(&mut self) -> &mut GHCB {
        self.ghcb
    }
}

#[cfg(dbg_assertions)]
impl Drop for GHCBRef {
    fn drop(&mut self) {
        unsafe {
            let cpu = this_cpu_unsafe();
            assert!(cpu.ghcb_consumer == this.consumer);
            cpu.ghcb_nesting_reference = false;
        }
    }
}

pub struct GHCBNesting {}

impl GHCBNesting {
    pub fn prepare_nested_ghcb(consumer: GHCBConsumer) {
        unsafe {
            let cpu = this_cpu_unsafe();
            #[cfg(dbg_assertions)]
            {
                assert!(cpu.ghcb_consumer != consumer);
                cpu.ghcb_consumer = consumer;
            }
        }
    }

    pub fn release_nested_ghcb(consumer: GHCBConsumer) {}

    pub fn nested_ghcb(consumer: GHCBConsumer) -> GHCBNestingRef {
        unsafe {
            let cpu = this_cpu_unsafe();
            #[cfg(dbg_assertions)]
            {
                assert!(cpu.ghcb_consumer == consumer);
                assert!(!cpu.ghcb_nesting_reference);
                cpu.ghcb_nesting_reference = true;
            }
            GHCBNestingRef {
                ghcb: cpu.nested_ghcb(consumer),
                consumer,
            }
        }
    }
}

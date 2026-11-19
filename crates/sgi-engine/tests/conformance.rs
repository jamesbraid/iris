//! The shared clauses, run on the mock board: they check the suite itself.

mod support;

use sgi_engine::conformance::Subject;
use sgi_engine::mock::{reg, Mock, MockConfig};
use support::Ram;

struct MockBoard;

impl Subject for MockBoard {
    type Engine = Mock<Ram>;

    fn engine(&self, notify: Option<Box<dyn Fn() + Send + Sync>>) -> Mock<Ram> {
        Mock::new(MockConfig::default(), Ram::new(), notify)
    }

    fn probe(&self) -> u32 {
        reg::IRQ
    }

    fn store(&self) -> (u32, u64) {
        (reg::DATA, 1)
    }

    fn port(&self) -> u32 {
        reg::DATA
    }

    fn region(&self) -> (u32, u32) {
        (0, 4)
    }

    fn missing_width(&self) -> Option<(u32, u32)> {
        None
    }

    fn holds_stores(&self) -> bool {
        true
    }

    fn credits(&self) -> bool {
        true
    }

    fn publish_words(&self, log: &sgi_engine::conformance::WriteLog) -> Vec<(u64, usize)> {
        use sgi_engine::BoardEngine;
        let mut e = Mock::new(MockConfig::default(), log.clone(), None);
        e.write(reg::DMA_DATA, 32, 0xbeef);
        e.write(reg::PUBLISH16, 32, 0x1002);
        e.write(reg::DMA_DATA, 32, 0xdead_beef);
        e.write(reg::PUBLISH32, 32, 0x2004);
        e.run(usize::MAX);
        vec![(0x1002, 2), (0x2004, 4)]
    }

    fn packet_storm(&self, e: &mut Mock<Ram>) -> Option<usize> {
        use sgi_engine::BoardEngine;
        for _ in 0..1000 {
            e.write(reg::PACKET, 32, 5);
            e.write(reg::PACKET_CLR, 32, 5);
            e.run(usize::MAX);
        }
        Some(1)
    }
}

sgi_engine::conformance_tests!(MockBoard);

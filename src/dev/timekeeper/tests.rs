use super::*;
use crate::traits::{BusDevice, Resettable, BUS_ERR};

#[test]
fn fuel_year_blank_storage_and_byte_width() {
    let t = Timekeeper::fuel_at(2026, 1, 2, 3, 4, 5).unwrap();
    assert_eq!(t.read8(0x7fff).data, 0x58);
    for a in [0, 0x100, 0x7f8, 0x7ff, 0x900, 0x7ff0] {
        assert_eq!(t.read8(a).data, 0);
    }
    t.write8(0x100, 0x12);
    t.write8(0x900, 0x34);
    assert_eq!(t.read8(0x100).data, 0x12);
    assert_eq!(t.read8(0x900).data, 0x34);
    assert_eq!(t.read32(0).status, BUS_ERR);
    assert_eq!(t.read8(0x8000).status, BUS_ERR);
    assert!(Timekeeper::fuel_at(1967, 1, 1, 0, 0, 0).is_err());
    assert!(Timekeeper::fuel_at(2068, 1, 1, 0, 0, 0).is_err());
}

#[test]
fn clock_freeze_stop_reset_and_rollover() {
    let t = Timekeeper::fuel_at(2067, 12, 31, 23, 59, 59).unwrap();
    t.write8(0x7f8, 0x42);
    t.write8(CONTROL as u64, 0x40);
    t.advance_for_test(2);
    assert_eq!(t.read8(0x7fff).data, 0x99);
    t.power_on();
    assert_eq!(t.read8(CONTROL as u64).data, 0);
    assert_eq!(t.read8(0x7fff).data, 0);
    assert_eq!(t.read8(0x7ffe).data, 1);
    assert_eq!(t.read8(0x7ffd).data, 1);
    assert_eq!(t.read8(0x7f8).data, 0x42);
    t.write8(CONTROL as u64, 0x80);
    t.write8(0x7ff9, 0x89);
    t.write8(CONTROL as u64, 0);
    t.advance_for_test(60);
    assert_eq!(t.read8(0x7ff9).data, 0x89);
}

#[test]
fn image_restore_preserves_bytes_and_elapsed_reference() {
    let t = Timekeeper::fuel_at(2026, 2, 28, 23, 59, 59).unwrap();
    t.write8(0x7ff0, 0xa5);
    let (image, reference) = t.persistence_image_at(100);
    let restored = Timekeeper::fuel_at(2026, 1, 1, 0, 0, 0).unwrap();
    restored
        .restore_image_at(&image, Some(&reference), 102)
        .unwrap();
    assert_eq!(restored.read8(0x7ffe).data, 3);
    assert_eq!(restored.read8(0x7ffd).data, 1);
    assert_eq!(restored.read8(0x7ff0).data, 0xa5);
    assert!(restored
        .restore_image_at(&image[..2048], None, 102)
        .is_err());
    let (after, _) = restored.persistence_image_at(102);
    assert_eq!(&after[..CONTROL], &image[..CONTROL]);
}

#[test]
fn leap_days_stopped_restore_and_reset_discards_staging() {
    for (year, month, day) in [(2024, 2, 0x29), (2026, 3, 1)] {
        let t = Timekeeper::fuel_at(year, 2, 28, 23, 59, 59).unwrap();
        t.advance_for_test(1);
        assert_eq!(t.read8(0x7ffe).data, month);
        assert_eq!(t.read8(0x7ffd).data, day);
        t.write8(CONTROL as u64, 0x80);
        t.write8(0x7fff, 0x12);
        t.power_on();
        assert_eq!(t.read8(0x7fff).data, bcd(year - 1968));
    }
    let t = Timekeeper::fuel_at(2026, 1, 1, 0, 0, 0).unwrap();
    t.write8(CONTROL as u64, 0x80);
    t.write8(0x7ff9, 0x85);
    t.write8(CONTROL as u64, 0);
    let (image, reference) = t.persistence_image_at(100);
    let restored = Timekeeper::fuel_at(2026, 1, 1, 0, 0, 0).unwrap();
    restored
        .restore_image_at(&image, Some(&reference), 1000)
        .unwrap();
    assert_eq!(restored.read8(0x7ff9).data, 0x85);
}

#[test]
fn backed_storage_is_blank_once_and_wrong_size_is_untouched() {
    let dir = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("timekeeper-test-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("nvram");
    let t = Timekeeper::fuel_at(2026, 1, 1, 0, 0, 0).unwrap();
    t.set_nvram_file(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), vec![0; SIZE]);
    t.write8(0x7f8, 0x42);
    let image = std::fs::read(&path).unwrap();
    assert_eq!(image[0x7f8], 0x42);
    assert_eq!(
        &image[CONTROL + 1..],
        &[0; 7],
        "RAM writes do not save clock cells"
    );
    t.flush_nvram().unwrap();
    let reopened = Timekeeper::fuel_at(2026, 1, 1, 0, 0, 0).unwrap();
    reopened.set_nvram_file(&path).unwrap();
    reopened.power_on();
    assert_eq!(reopened.read8(0x7f8).data, 0x42);
    drop(reopened);
    drop(t);
    let wrong = dir.join("wrong");
    std::fs::write(&wrong, [0xa5; 2048]).unwrap();
    let t = Timekeeper::fuel_at(2026, 1, 1, 0, 0, 0).unwrap();
    assert!(t.set_nvram_file(&wrong).is_err());
    assert_eq!(std::fs::read(&wrong).unwrap(), vec![0xa5; 2048]);
    assert!(!dir.join("wrong.clock").exists());
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(dir.join("nvram.clock")).unwrap();
    std::fs::remove_file(wrong).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

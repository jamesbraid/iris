#[test]
fn adds_wrongly() {
    println!("stdout line from failing test");
    assert_eq!(spike::add(2, 2), 5, "two plus two is not five");
}

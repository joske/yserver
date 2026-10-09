use super::largest_free_xid_gap;

#[test]
fn largest_free_xid_gap_cases() {
    let base = 0x0010_0000u32;
    let mask = 0x000F_FFFFu32;
    let hi = base | mask;
    // empty range → whole range (lo = base since base != 0)
    assert_eq!(largest_free_xid_gap(base, mask, &[]), (base, mask + 1));
    // one used id at the start → gap after it
    assert_eq!(largest_free_xid_gap(base, mask, &[base]), (base + 1, mask));
    // split: small gap low, big gap high → picks the big one
    let used: Vec<u32> = (base..base + 10).chain([base + 12]).collect();
    assert_eq!(
        largest_free_xid_gap(base, mask, &used),
        (base + 13, hi - (base + 13) + 1)
    );
    // fully exhausted → Xorg wire shape (0, 1)
    let all: Vec<u32> = (base..=hi).collect(); // NOTE: 1M entries — fine in a test
    assert_eq!(largest_free_xid_gap(base, mask, &all), (0, 1));
    // base 0 (hypothetical) excludes XID 0
    assert_eq!(largest_free_xid_gap(0, 0xFF, &[]), (1, 0xFF));
}

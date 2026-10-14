



pub fn mul_div_ceil(a: u64, b: u64, c: u64) -> u64 {
    if c == 0 {
        return 0;
    }
    let c = c as u128;
    ((a as u128 * b as u128).div_ceil(c)).min(u64::MAX as u128) as u64
}


pub fn constant_product_out(amount_in: u64, reserve_in: u64, reserve_out: u64) -> u64 {
    let denominator = reserve_in as u128 + amount_in as u128;
    if denominator == 0 {
        return 0;
    }
    ((reserve_out as u128 * amount_in as u128) / denominator) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_product_rounds_down() {
        let out = constant_product_out(1_000_000, 30_000_000_000, 1_073_000_000_000_000);
        assert_eq!(out, 35_765_474_484);
        assert_eq!(constant_product_out(1, 0, 0), 0);
    }

    #[test]
    fn mul_div_rounds_up() {
        assert_eq!(mul_div_ceil(10, 3, 4), 8);
        assert_eq!(mul_div_ceil(8, 1, 4), 2);
        assert_eq!(mul_div_ceil(1, 1, 0), 0);
    }
}

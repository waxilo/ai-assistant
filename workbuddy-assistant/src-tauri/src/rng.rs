//! 轻量随机数：只服务于「把请求节奏打散」这一类需求。
//!
//! 为什么不用 `rand`：多账号风控要的是**不可预测 + 别重复**，不是密码学强度——
//! 签到顺序、间隔秒数、当天触发时刻都是节奏层面的噪声，被算出规律也无所谓。
//! 为此拉一个依赖进来不值当，所以这里用「纳秒时钟 ^ 单调计数器」播种 + splitmix64
//! 终混，几十行搞定，且同一纳秒内连续调用也会拿到不同值（纯时钟播种做不到这点，
//! 而批量签到恰恰会在同一毫秒里连取好几个数）。
//!
//! 所有函数都不持有锁、不访问文件，纯计算，可放心在任意线程调用。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 单调计数器：与时钟一起充当种子，保证连续调用不撞同一个值
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// splitmix64 的终混步骤：把低位的不规律铺满整个字长
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    x
}

fn next_u64() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // 计数器乘一个奇常数再异或，避免「时钟没走 + 计数器 +1」的规律性
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    mix(nanos ^ seq)
}

/// `[0, n)` 均匀取值。`n == 0` 返回 0（调用方用 `max < 2` 挡掉退化输入，这里不 panic）。
pub fn below(n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        next_u64() % n
    }
}

/// 闭区间 `[lo, hi]` 取值。传反了自动交换——打散逻辑里上下界本就容易写混，
/// 与其 panic 不如按「区间」的本意处理。
pub fn range(lo: u64, hi: u64) -> u64 {
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    lo + below(hi - lo + 1)
}

/// Fisher–Yates 就地打乱。长度 < 2 时什么都不做。
pub fn shuffle<T>(items: &mut [T]) {
    if items.len() < 2 {
        return;
    }
    for i in (1..items.len()).rev() {
        items.swap(i, below(i as u64 + 1) as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_stays_in_range_and_survives_zero() {
        assert_eq!(below(0), 0, "0 上界应返回 0 而不是 panic");
        assert_eq!(below(1), 0);
        for _ in 0..200 {
            assert!(below(7) < 7);
        }
    }

    #[test]
    fn range_is_inclusive_and_order_insensitive() {
        assert_eq!(range(5, 5), 5);
        for _ in 0..200 {
            let v = range(3, 6);
            assert!((3..=6).contains(&v), "越界：{v}");
            let w = range(6, 3); // 上下界传反
            assert!((3..=6).contains(&w), "越界：{w}");
        }
    }

    #[test]
    fn consecutive_calls_do_not_repeat() {
        // 纯时钟播种在同一纳秒内会连出同一个数——这正是批量签到会遇到的场景
        let vals: std::collections::HashSet<u64> = (0..64).map(|_| below(1_000_000)).collect();
        assert!(vals.len() > 60, "连续取值重复过多：{} 个不同值", vals.len());
    }

    #[test]
    fn shuffle_keeps_everyone_and_actually_shuffles() {
        let mut empty: Vec<u32> = Vec::new();
        shuffle(&mut empty);
        assert!(empty.is_empty());

        let mut one = vec![1];
        shuffle(&mut one);
        assert_eq!(one, vec![1]);

        // 10 个元素：单调递增的输入若被原样留着，说明打乱没生效；
        // 200 次里出现一次乱序的概率是 1 - (1/10!)^200，实际上必然发生
        let mut shuffled_once = false;
        for _ in 0..200 {
            let mut v: Vec<u32> = (0..10).collect();
            shuffle(&mut v);
            let mut sorted = v.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..10).collect::<Vec<u32>>(), "元素被弄丢了：{v:?}");
            if v != (0..10).collect::<Vec<u32>>() {
                shuffled_once = true;
            }
        }
        assert!(shuffled_once, "200 次都没有改变顺序，打乱没有生效");
    }
}

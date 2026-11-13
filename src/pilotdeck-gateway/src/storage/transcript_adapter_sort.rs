//! Node 22 / V8 12.4 stable sorting for a comparator that may return NaN.
//!
//! Adapted from V8 12.4.254 `third_party/v8/builtins/array-sort.tq`, itself
//! derived from CPython TimSort. Copyright (c) 2001–2018 Python Software
//! Foundation; all rights reserved. The Python material is subject to the
//! Python Software Foundation License Version 2; applicable V8 project
//! material is subject to the V8 BSD 3-clause licence. The retained notices,
//! full licence texts, source links and modification summary are in
//! `scripts/backend-release/license-supplements/README.md`.
//!
//! ECMAScript treats a NaN comparator result as equality. That comparator is
//! not transitive: a general Rust sort may panic or produce another order.

use pilotdeck_protocol::JsValue;

#[derive(Clone, Copy)]
struct Item {
    value: f64,
    index: usize,
}
fn less(a: Item, b: Item) -> bool {
    // TS compares right.lastModified - left.lastModified. NaN is treated as 0.
    (b.value - a.value) < 0.0
}

struct Sorter {
    items: Vec<Item>,
    runs: Vec<(usize, usize)>,
    min_gallop: usize,
}
impl Sorter {
    fn gallop(
        items: &[Item],
        key: Item,
        base: usize,
        len: usize,
        hint: usize,
        right: bool,
    ) -> usize {
        let is_less = |x: Item| if right { less(key, x) } else { less(x, key) };
        let go_right = if right {
            !is_less(items[base + hint])
        } else {
            is_less(items[base + hint])
        };
        let (mut last, mut offset) = (0usize, 1usize);
        let (mut lo, mut hi): (isize, usize);
        if go_right {
            let max = len - hint;
            while offset < max
                && if right {
                    !is_less(items[base + hint + offset])
                } else {
                    is_less(items[base + hint + offset])
                }
            {
                last = offset;
                offset = offset.saturating_mul(2).saturating_add(1);
            }
            lo = (last + hint) as isize;
            hi = offset.min(max) + hint;
        } else {
            let max = hint + 1;
            while offset < max
                && if right {
                    is_less(items[base + hint - offset])
                } else {
                    !is_less(items[base + hint - offset])
                }
            {
                last = offset;
                offset = offset.saturating_mul(2).saturating_add(1);
            }
            lo = hint as isize - offset.min(max) as isize;
            hi = hint - last;
        }
        lo += 1;
        let mut lo = lo as usize;
        while lo < hi {
            let middle = lo + (hi - lo) / 2;
            if if right {
                is_less(items[base + middle])
            } else {
                !is_less(items[base + middle])
            } {
                hi = middle;
            } else {
                lo = middle + 1;
            }
        }
        hi
    }
    fn copy_slice(items: &mut [Item], from: usize, to: usize, len: usize) {
        items.copy_within(from..from + len, to);
    }
    fn low(&mut self, base_a: usize, mut la: usize, base_b: usize, mut lb: usize) {
        let tmp = self.items[base_a..base_a + la].to_vec();
        let (mut i, mut j, mut d) = (0, base_b, base_a);
        self.items[d] = self.items[j];
        d += 1;
        j += 1;
        lb -= 1;
        let mut gallop = self.min_gallop;
        loop {
            if lb == 0 {
                self.items[d..d + la].copy_from_slice(&tmp[i..i + la]);
                return;
            }
            if la == 1 {
                Self::copy_slice(&mut self.items, j, d, lb);
                self.items[d + lb] = tmp[i];
                return;
            }
            let (mut wa, mut wb) = (0, 0);
            loop {
                if less(self.items[j], tmp[i]) {
                    self.items[d] = self.items[j];
                    d += 1;
                    j += 1;
                    lb -= 1;
                    wb += 1;
                    wa = 0;
                    if lb == 0 {
                        self.items[d..d + la].copy_from_slice(&tmp[i..i + la]);
                        return;
                    }
                    if wb >= gallop {
                        break;
                    }
                } else {
                    self.items[d] = tmp[i];
                    d += 1;
                    i += 1;
                    la -= 1;
                    wa += 1;
                    wb = 0;
                    if la == 1 {
                        Self::copy_slice(&mut self.items, j, d, lb);
                        self.items[d + lb] = tmp[i];
                        return;
                    }
                    if wa >= gallop {
                        break;
                    }
                }
            }
            gallop += 1;
            let mut first = true;
            while wa >= 7 || wb >= 7 || first {
                first = false;
                gallop = gallop.saturating_sub(1).max(1);
                self.min_gallop = gallop;
                wa = Self::gallop(&tmp, self.items[j], i, la, 0, true);
                if wa > 0 {
                    self.items[d..d + wa].copy_from_slice(&tmp[i..i + wa]);
                    d += wa;
                    i += wa;
                    la -= wa;
                    if la == 1 {
                        Self::copy_slice(&mut self.items, j, d, lb);
                        self.items[d + lb] = tmp[i];
                        return;
                    }
                    if la == 0 {
                        return;
                    }
                }
                self.items[d] = self.items[j];
                d += 1;
                j += 1;
                lb -= 1;
                if lb == 0 {
                    self.items[d..d + la].copy_from_slice(&tmp[i..i + la]);
                    return;
                }
                wb = Self::gallop(&self.items, tmp[i], j, lb, 0, false);
                if wb > 0 {
                    Self::copy_slice(&mut self.items, j, d, wb);
                    d += wb;
                    j += wb;
                    lb -= wb;
                    if lb == 0 {
                        self.items[d..d + la].copy_from_slice(&tmp[i..i + la]);
                        return;
                    }
                }
                self.items[d] = tmp[i];
                d += 1;
                i += 1;
                la -= 1;
                if la == 1 {
                    Self::copy_slice(&mut self.items, j, d, lb);
                    self.items[d + lb] = tmp[i];
                    return;
                }
            }
            gallop += 1;
            self.min_gallop = gallop;
        }
    }
    fn high(&mut self, base_a: usize, mut la: usize, base_b: usize, mut lb: usize) {
        let tmp = self.items[base_b..base_b + lb].to_vec();
        // Signed cursors allow the final move to step just before the range.
        let (mut i, mut j, mut d) = (
            (base_a + la - 1) as isize,
            (lb - 1) as isize,
            (base_b + lb - 1) as isize,
        );
        self.items[d as usize] = self.items[i as usize];
        d -= 1;
        i -= 1;
        la -= 1;
        let mut gallop = self.min_gallop;
        loop {
            if la == 0 {
                let at = (d - lb as isize + 1) as usize;
                self.items[at..at + lb].copy_from_slice(&tmp[..lb]);
                return;
            }
            if lb == 1 {
                d -= la as isize;
                i -= la as isize;
                Self::copy_slice(&mut self.items, (i + 1) as usize, (d + 1) as usize, la);
                self.items[d as usize] = tmp[j as usize];
                return;
            }
            let (mut wa, mut wb) = (0, 0);
            loop {
                if less(tmp[j as usize], self.items[i as usize]) {
                    self.items[d as usize] = self.items[i as usize];
                    d -= 1;
                    i -= 1;
                    la -= 1;
                    wa += 1;
                    wb = 0;
                    if la == 0 {
                        let at = (d - lb as isize + 1) as usize;
                        self.items[at..at + lb].copy_from_slice(&tmp[..lb]);
                        return;
                    }
                    if wa >= gallop {
                        break;
                    }
                } else {
                    self.items[d as usize] = tmp[j as usize];
                    d -= 1;
                    j -= 1;
                    lb -= 1;
                    wb += 1;
                    wa = 0;
                    if lb == 1 {
                        d -= la as isize;
                        i -= la as isize;
                        Self::copy_slice(&mut self.items, (i + 1) as usize, (d + 1) as usize, la);
                        self.items[d as usize] = tmp[j as usize];
                        return;
                    }
                    if wb >= gallop {
                        break;
                    }
                }
            }
            gallop += 1;
            let mut first = true;
            while wa >= 7 || wb >= 7 || first {
                first = false;
                gallop = gallop.saturating_sub(1).max(1);
                self.min_gallop = gallop;
                wa = la - Self::gallop(&self.items, tmp[j as usize], base_a, la, la - 1, true);
                if wa > 0 {
                    d -= wa as isize;
                    i -= wa as isize;
                    Self::copy_slice(&mut self.items, (i + 1) as usize, (d + 1) as usize, wa);
                    la -= wa;
                    if la == 0 {
                        let at = (d - lb as isize + 1) as usize;
                        self.items[at..at + lb].copy_from_slice(&tmp[..lb]);
                        return;
                    }
                }
                self.items[d as usize] = tmp[j as usize];
                d -= 1;
                j -= 1;
                lb -= 1;
                if lb == 1 {
                    d -= la as isize;
                    i -= la as isize;
                    Self::copy_slice(&mut self.items, (i + 1) as usize, (d + 1) as usize, la);
                    self.items[d as usize] = tmp[j as usize];
                    return;
                }
                wb = lb - Self::gallop(&tmp, self.items[i as usize], 0, lb, lb - 1, false);
                if wb > 0 {
                    d -= wb as isize;
                    j -= wb as isize;
                    self.items[(d + 1) as usize..(d + 1) as usize + wb]
                        .copy_from_slice(&tmp[(j + 1) as usize..(j + 1) as usize + wb]);
                    lb -= wb;
                    if lb == 1 {
                        d -= la as isize;
                        i -= la as isize;
                        Self::copy_slice(&mut self.items, (i + 1) as usize, (d + 1) as usize, la);
                        self.items[d as usize] = tmp[j as usize];
                        return;
                    }
                    if lb == 0 {
                        return;
                    }
                }
                self.items[d as usize] = self.items[i as usize];
                d -= 1;
                i -= 1;
                la -= 1;
                if la == 0 {
                    let at = (d - lb as isize + 1) as usize;
                    self.items[at..at + lb].copy_from_slice(&tmp[..lb]);
                    return;
                }
            }
            gallop += 1;
            self.min_gallop = gallop;
        }
    }
    fn merge(&mut self, k: usize) {
        let (mut a, mut la) = self.runs[k];
        let (b, mut lb) = self.runs[k + 1];
        self.runs.splice(k..k + 2, [(a, la + lb)]);
        let skip = Self::gallop(&self.items, self.items[b], a, la, 0, true);
        a += skip;
        la -= skip;
        if la == 0 {
            return;
        }
        lb = Self::gallop(&self.items, self.items[a + la - 1], b, lb, lb - 1, false);
        if lb == 0 {
            return;
        }
        if la <= lb {
            self.low(a, la, b, lb);
        } else {
            self.high(a, la, b, lb);
        }
    }
    fn collapse(&mut self) {
        while self.runs.len() > 1 {
            let mut n = self.runs.len() - 2;
            if (n > 0 && self.runs[n - 1].1 <= self.runs[n].1 + self.runs[n + 1].1)
                || (n > 1 && self.runs[n - 2].1 <= self.runs[n - 1].1 + self.runs[n].1)
            {
                if self.runs[n - 1].1 < self.runs[n + 1].1 {
                    n -= 1;
                }
                self.merge(n);
            } else if self.runs[n].1 <= self.runs[n + 1].1 {
                self.merge(n);
            } else {
                break;
            }
        }
    }
    fn sort(&mut self) {
        let n = self.items.len();
        let mut min = n;
        let mut remainder = 0;
        while min >= 64 {
            remainder |= min & 1;
            min >>= 1;
        }
        min += remainder;
        let mut base = 0;
        while base < n {
            let mut end = base + 1;
            if end < n {
                let down = less(self.items[end], self.items[base]);
                end += 1;
                while end < n
                    && (if down {
                        less(self.items[end], self.items[end - 1])
                    } else {
                        !less(self.items[end], self.items[end - 1])
                    })
                {
                    end += 1;
                }
                if down {
                    self.items[base..end].reverse();
                }
            }
            let target = n.min(base + min.max(end - base));
            for pos in end..target {
                let value = self.items[pos];
                let (mut lo, mut hi) = (base, pos);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if less(value, self.items[mid]) {
                        hi = mid;
                    } else {
                        lo = mid + 1;
                    }
                }
                self.items.copy_within(lo..pos, lo + 1);
                self.items[lo] = value;
            }
            self.runs.push((base, target - base));
            self.collapse();
            base = target;
        }
        while self.runs.len() > 1 {
            let mut k = self.runs.len() - 2;
            if k > 0 && self.runs[k - 1].1 < self.runs[k + 1].1 {
                k -= 1;
            }
            self.merge(k);
        }
    }
}

pub(super) fn sort_sessions(sessions: &mut Vec<(f64, JsValue)>) {
    let items = sessions
        .iter()
        .enumerate()
        .map(|(index, (value, _))| Item {
            value: *value,
            index,
        })
        .collect();
    let mut sorter = Sorter {
        items,
        runs: Vec::new(),
        min_gallop: 7,
    };
    sorter.sort();
    let mut originals: Vec<_> = sessions.drain(..).map(Some).collect();
    sessions.extend(
        sorter
            .items
            .into_iter()
            .map(|item| originals[item.index].take().expect("unique sort index")),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(values: &[f64]) -> Vec<usize> {
        let mut sessions = values
            .iter()
            .enumerate()
            .map(|(i, v)| (*v, JsValue::Number(i as f64)))
            .collect();
        sort_sessions(&mut sessions);
        sessions
            .into_iter()
            .map(|(_, v)| v.as_number().unwrap() as usize)
            .collect()
    }

    #[test]
    fn nan_order_matches_fixed_node22_list_sessions_oracle() {
        assert_eq!(order(&[1.0, 2.0, f64::NAN, 4.0, 3.0]), [3, 4, 1, 0, 2]);
        assert_eq!(order(&[3.0, 2.0, f64::NAN, 4.0]), [0, 1, 2, 3]);
        let values: Vec<_> = (0..65)
            .map(|i| {
                if i % 7 == 0 {
                    f64::NAN
                } else {
                    ((i * 37) % 59) as f64
                }
            })
            .collect();
        assert_eq!(
            order(&values),
            [
                0, 27, 19, 11, 3, 30, 22, 6, 25, 17, 9, 1, 20, 12, 4, 31, 23, 15, 2, 5, 7, 26, 18,
                10, 29, 13, 32, 24, 16, 8, 14, 21, 28, 33, 34, 35, 51, 43, 62, 54, 46, 38, 57, 41,
                60, 52, 44, 36, 55, 47, 39, 58, 50, 61, 53, 45, 37, 48, 40, 42, 49, 56, 64, 59, 63
            ]
        );
    }
}

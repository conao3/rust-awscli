//! `difflib.get_close_matches` (Python 3.14), used for the "Maybe you
//! meant" suggestions of `awscli/argparser.py`.

use std::collections::HashMap;

/// `SequenceMatcher(None, a, b)` reduced to what `ratio()` needs.
struct Matcher<'a> {
    a: &'a [char],
    b: &'a [char],
    b2j: HashMap<char, Vec<usize>>,
}

impl<'a> Matcher<'a> {
    fn new(a: &'a [char], b: &'a [char]) -> Matcher<'a> {
        let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
        for (i, &c) in b.iter().enumerate() {
            b2j.entry(c).or_default().push(i);
        }
        let n = b.len();
        if n >= 200 {
            let ntest = n / 100 + 1;
            b2j.retain(|_, idxs| idxs.len() <= ntest);
        }
        Matcher { a, b, b2j }
    }

    /// `find_longest_match` without junk.
    fn find_longest_match(
        &self,
        alo: usize,
        ahi: usize,
        blo: usize,
        bhi: usize,
    ) -> (usize, usize, usize) {
        let (a, b) = (self.a, self.b);
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, c) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(js) = self.b2j.get(c) {
                for &j in js {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j
                        .checked_sub(1)
                        .and_then(|p| j2len.get(&p))
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && a[besti + bestsize] == b[bestj + bestsize]
        {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    /// The total size of `get_matching_blocks()`.
    fn matches(&self) -> usize {
        let mut queue = vec![(0, self.a.len(), 0, self.b.len())];
        let mut total = 0;
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.find_longest_match(alo, ahi, blo, bhi);
            if k > 0 {
                total += k;
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        total
    }

    fn ratio(&self) -> f64 {
        let length = self.a.len() + self.b.len();
        if length == 0 {
            return 1.0;
        }
        2.0 * self.matches() as f64 / length as f64
    }
}

/// `difflib.get_close_matches(word, possibilities, n, cutoff)`.
pub(crate) fn get_close_matches<S: AsRef<str>>(
    word: &str,
    possibilities: &[S],
    n: usize,
    cutoff: f64,
) -> Vec<String> {
    let b: Vec<char> = word.chars().collect();
    let mut result: Vec<(f64, String)> = Vec::new();
    for x in possibilities {
        let a: Vec<char> = x.as_ref().chars().collect();
        let ratio = Matcher::new(&a, &b).ratio();
        if ratio >= cutoff {
            result.push((ratio, x.as_ref().to_string()));
        }
    }
    result.sort_by(|l, r| r.0.total_cmp(&l.0).then_with(|| r.1.cmp(&l.1)));
    result.truncate(n);
    result.into_iter().map(|(_, x)| x).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_python() {
        assert_eq!(
            get_close_matches("appel", &["ape", "apple", "peach", "puppy"], 3, 0.6),
            ["apple", "ape"]
        );
        assert_eq!(
            get_close_matches(
                "describe-instancs",
                &[
                    "describe-instances",
                    "describe-instance-types",
                    "describe-instance-status",
                    "run-instances"
                ],
                3,
                0.8
            ),
            [
                "describe-instances",
                "describe-instance-types",
                "describe-instance-status"
            ]
        );
    }
}

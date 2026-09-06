//! Line-based unified diff for showing file edits.

use std::fmt::Write as _;

/// Longest lines that get the O(n·m) treatment; beyond that the changed
/// middle is emitted as one delete/insert pair.
const DP_LIMIT: usize = 4_000_000;
/// Cap on emitted lines so a large rewrite does not flood the transcript.
const MAX_LINES: usize = 400;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Ins,
}

/// Unified diff of `old` → `new` with `context` lines around each change and
/// 1-based line numbers in the hunk headers. Empty when the texts are equal.
pub fn unified(old: &str, new: &str, context: usize) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = script(&a, &b);
    if ops.iter().all(|&o| o == Op::Keep) {
        return String::new();
    }
    let mut out = String::new();
    let mut emitted = 0usize;
    let mut i = 0usize;
    let (mut ai, mut bi) = (0usize, 0usize);
    // walk the script, grouping changes into hunks
    while i < ops.len() {
        if ops[i] == Op::Keep {
            ai += 1;
            bi += 1;
            i += 1;
            continue;
        }
        // hunk start: back up `context` keeps
        let mut start = i;
        let mut ctx_before = 0;
        while start > 0 && ops[start - 1] == Op::Keep && ctx_before < context {
            start -= 1;
            ctx_before += 1;
        }
        // hunk end: extend past the last change while gaps are ≤ 2·context
        let mut end = i;
        let mut last_change = i;
        while end < ops.len() {
            if ops[end] != Op::Keep {
                last_change = end;
            } else if end - last_change > 2 * context {
                break;
            }
            end += 1;
        }
        let end = (last_change + 1 + context).min(ops.len());
        let (a0, b0) = (ai - ctx_before, bi - ctx_before);
        let mut lines = Vec::new();
        let (mut an, mut bn) = (0usize, 0usize);
        let (mut aj, mut bj) = (a0, b0);
        for &op in &ops[start..end] {
            match op {
                Op::Keep => {
                    lines.push(format!(" {}", a[aj]));
                    aj += 1;
                    bj += 1;
                    an += 1;
                    bn += 1;
                }
                Op::Del => {
                    lines.push(format!("-{}", a[aj]));
                    aj += 1;
                    an += 1;
                }
                Op::Ins => {
                    lines.push(format!("+{}", b[bj]));
                    bj += 1;
                    bn += 1;
                }
            }
        }
        let _ = writeln!(out, "@@ -{},{an} +{},{bn} @@", a0 + 1, b0 + 1);
        for l in lines {
            if emitted >= MAX_LINES {
                let _ = writeln!(out, "… diff truncated");
                return out;
            }
            out.push_str(&l);
            out.push('\n');
            emitted += 1;
        }
        ai = aj;
        bi = bj;
        i = end;
    }
    out
}

fn script(a: &[&str], b: &[&str]) -> Vec<Op> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (ma, mb) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut ops = vec![Op::Keep; pre];
    if ma.len() * mb.len() > DP_LIMIT {
        ops.extend(std::iter::repeat_n(Op::Del, ma.len()));
        ops.extend(std::iter::repeat_n(Op::Ins, mb.len()));
    } else {
        ops.extend(lcs_ops(ma, mb));
    }
    ops.extend(std::iter::repeat_n(Op::Keep, suf));
    ops
}

/// Classic LCS table, then backtrack; deletions before insertions.
fn lcs_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    let w = m + 1;
    let mut t = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i * w + j] = if a[i] == b[j] {
                t[(i + 1) * w + j + 1] + 1
            } else {
                t[(i + 1) * w + j].max(t[i * w + j + 1])
            };
        }
    }
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Keep);
            i += 1;
            j += 1;
        } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
            ops.push(Op::Del);
            i += 1;
        } else {
            ops.push(Op::Ins);
            j += 1;
        }
    }
    ops.extend(std::iter::repeat_n(Op::Del, n - i));
    ops.extend(std::iter::repeat_n(Op::Ins, m - j));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_and_numbers() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nX\nd\ne\nf\ng\n";
        assert_eq!(unified(old, new, 1), "@@ -2,3 +2,3 @@\n b\n-c\n+X\n d\n");
        assert_eq!(unified(old, old, 1), "");
        assert_eq!(unified("", "x\ny\n", 2), "@@ -1,0 +1,2 @@\n+x\n+y\n");
    }

    #[test]
    fn separate_hunks_when_far_apart() {
        let old: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let new = old
            .replace("\n3\n", "\nthree\n")
            .replace("\n18\n", "\neighteen\n");
        let d = unified(&old, &new, 1);
        assert_eq!(d.matches("@@").count(), 4, "{d}");
        assert!(d.contains("@@ -2,3 +2,3 @@\n 2\n-3\n+three\n 4\n"));
        assert!(d.contains("@@ -17,3 +17,3 @@\n 17\n-18\n+eighteen\n 19\n"));
    }
}

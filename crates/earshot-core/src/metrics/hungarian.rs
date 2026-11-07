//! Optimal assignment (Hungarian / Kuhn-Munkres algorithm).
//!
//! O(n^2 m) shortest-augmenting-path formulation with potentials, after H. W. Kuhn,
//! "The Hungarian method for the assignment problem" (1955) and the well-known
//! e-maxx.ru implementation ("Hungarian algorithm for the assignment problem").

/// Returns, for each row of `weights` (rows x cols, all rows the same length), the
/// column it is assigned to so that the total weight is maximal. Each column is used at
/// most once; with more rows than columns some rows get `None`.
pub fn max_weight_assignment(weights: &[Vec<f64>]) -> Vec<Option<usize>> {
    let rows = weights.len();
    let cols = weights.first().map_or(0, Vec::len);
    if rows == 0 || cols == 0 {
        return vec![None; rows];
    }
    if rows <= cols {
        let cost: Vec<Vec<f64>> = weights
            .iter()
            .map(|r| r.iter().map(|w| -w).collect())
            .collect();
        min_cost_rows_le_cols(&cost)
    } else {
        // Transpose so rows <= cols, then invert the assignment.
        let cost: Vec<Vec<f64>> = (0..cols)
            .map(|c| weights.iter().map(|r| -r[c]).collect())
            .collect();
        let by_col = min_cost_rows_le_cols(&cost);
        let mut by_row = vec![None; rows];
        for (c, r) in by_col.iter().enumerate() {
            if let Some(r) = r {
                by_row[*r] = Some(c);
            }
        }
        by_row
    }
}

/// Minimum-cost assignment of every row to a distinct column; requires rows <= cols.
fn min_cost_rows_le_cols(a: &[Vec<f64>]) -> Vec<Option<usize>> {
    let n = a.len();
    let m = a[0].len();
    debug_assert!(n <= m);
    // 1-indexed potentials and matching, index 0 is a sentinel.
    let mut u = vec![0.0f64; n + 1];
    let mut v = vec![0.0f64; m + 1];
    let mut p = vec![0usize; m + 1]; // p[j] = row matched to column j
    let mut way = vec![0usize; m + 1];
    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0usize;
        let mut minv = vec![f64::INFINITY; m + 1];
        let mut used = vec![false; m + 1];
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = f64::INFINITY;
            let mut j1 = 0usize;
            for j in 1..=m {
                if !used[j] {
                    let cur = a[i0 - 1][j - 1] - u[i0] - v[j];
                    if cur < minv[j] {
                        minv[j] = cur;
                        way[j] = j0;
                    }
                    if minv[j] < delta {
                        delta = minv[j];
                        j1 = j;
                    }
                }
            }
            for j in 0..=m {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }
    let mut ans = vec![None; n];
    for j in 1..=m {
        if p[j] != 0 {
            ans[p[j] - 1] = Some(j - 1);
        }
    }
    ans
}

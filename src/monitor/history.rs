//! 時間窓つきの時系列を、描画幅（列数）に合わせて集約する。
//!
//! モニターペインのグラフは 5 分などの固定の時間窓を表示する。「1 列 = 1 サンプル」にすると、
//! ペインの幅で表示される時間の長さが変わり、幅が変わるたびにグラフの読み方が変わってしまう。
//! ここでは常に `[now − window, now]` を列数で等分し、各列に重なった計測の区間を、重なった
//! 時間で重み付けして集約する。
//!
//! 各点は「直前の点からその時刻まで」の区間を代表する値として扱う。CPU 使用率は前回の計測からの
//! 平均なので、この解釈がそのまま正しい。点の間隔が想定の 3 倍を超えたとき（スリープからの
//! 復帰や停止）はその間を欠測とし、後ろの点には自分の直前の `nominal_interval` だけを代表させる。
//! 欠測を前後の値で埋めると、計測していない時間帯にも値があったように見えてしまう。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 点の間隔が `nominal_interval` のこの倍数を超えたら、その間を欠測とみなす。
///
/// 負荷で 1 秒ごとの計測が 1〜2 回遅れた程度は連続した区間として扱い、それより長い空白
/// （スリープ・停止）だけを欠測にする。
const GAP_FACTOR: u32 = 3;

/// 1 回の計測が代表する区間 `(end − span, end]` と、その値。
#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    /// 区間の終わり（計測した時刻）。
    end: Instant,
    /// 区間の長さ。直前の点までの間隔で、最初の点と欠測の直後の点は `nominal_interval`。
    ///
    /// 始まりを `Instant` で持たないのは、単調時計の起点の近く（起動直後）では
    /// `end − nominal_interval` を表せない環境があるため。
    span: Duration,
    value: f64,
}

/// 時間窓つきの時系列。各点は「直前の点からその時刻まで」の区間を代表する値
/// （CPU 使用率は区間の平均なので、この解釈が正しい）。
#[derive(Clone, Debug)]
pub struct Series {
    window: Duration,
    nominal_interval: Duration,
    /// 時刻の昇順。区間は互いに重ならない（各区間は直前の点の時刻以降に始まる）。
    points: VecDeque<Point>,
}

impl Series {
    /// `window`: 表示する時間幅（例 5 分）。`nominal_interval`: 想定の計測間隔（例 1 秒）。
    /// 点と点の間隔が nominal_interval の 3 倍を超えたら、その間は欠測（スリープ復帰・停止）とみなし、
    /// 後ろの点は「その点の時刻 − nominal_interval」からの区間だけを代表する。
    ///
    /// `nominal_interval` が 0 だと、どの点も長さ 0 の区間になり、列は常に None になる。
    pub fn new(window: Duration, nominal_interval: Duration) -> Self {
        Self {
            window,
            nominal_interval,
            points: VecDeque::new(),
        }
    }

    /// 時刻は単調増加で渡される前提。古くなった点（now − window より前に終わる区間）は捨てる。
    /// 同時刻・逆行する点は捨てる。
    ///
    /// 有限でない値（NaN・無限大）も捨てる。1 つでも混ざると、その点に重なる列の平均が NaN に
    /// なるため。捨てた回は計測しなかった回と同じ扱いになり、次の点がその区間も代表する。
    pub fn push(&mut self, at: Instant, value: f64) {
        if !value.is_finite() {
            return;
        }
        let span = match self.points.back() {
            Some(last) if at <= last.end => return,
            Some(last) => {
                let gap = at.duration_since(last.end);
                if gap > self.nominal_interval.saturating_mul(GAP_FACTOR) {
                    self.nominal_interval
                } else {
                    gap
                }
            }
            None => self.nominal_interval,
        };
        self.points.push_back(Point {
            end: at,
            span,
            value,
        });
        // 今回の時刻を now とみなす。区間が now − window より前に終わった点は、以後どの列にも重ならない。
        while self
            .points
            .front()
            .is_some_and(|oldest| at.duration_since(oldest.end) > self.window)
        {
            self.points.pop_front();
        }
    }

    /// 最後に記録した値。
    pub fn latest(&self) -> Option<f64> {
        self.points.back().map(|point| point.value)
    }

    /// [now − window, now] を columns 個の等しい区間に分け、各区間に重なる点の区間を、重なった時間で
    /// 重み付けした平均。データの無い区間（記録開始前・欠測）は None。columns == 0 なら空。
    pub fn columns_mean(&self, now: Instant, columns: usize) -> Vec<Option<f64>> {
        // 列ごとに（ここまでの平均, 重なった時間の合計）を持ち、点が来るたびに平均を重みの割合だけ
        // 動かす。値 × 時間 [ns] の総和を持つと、有限でも大きな値（1e300 × 1e9 など）で総和が
        // 無限大になるため。
        let mut means = vec![(0.0_f64, 0_u128); columns];
        self.for_each_overlap(now, columns, |column, value, overlap_nanos| {
            let (mean, weight) = &mut means[column];
            *weight += overlap_nanos;
            *mean += (value - *mean) * (overlap_nanos as f64 / *weight as f64);
        });
        means
            .into_iter()
            .map(|(mean, weight)| (weight > 0).then_some(mean))
            .collect()
    }

    /// 同じ区間分けで、重なる点の最大値（逼迫度のような段階の値に使う）。
    pub fn columns_max(&self, now: Instant, columns: usize) -> Vec<Option<f64>> {
        let mut maxima: Vec<Option<f64>> = vec![None; columns];
        self.for_each_overlap(now, columns, |column, value, _| {
            let slot = &mut maxima[column];
            *slot = Some(slot.map_or(value, |max| max.max(value)));
        });
        maxima
    }

    /// `[now − window, now]` を `columns` 個に等分し、点の区間と重なる列ごとに
    /// `(列, 値, 重なった時間 [ns])` を渡す。重なりが 0 の組は渡さない。
    ///
    /// 位置は窓の始まりから測った ns の整数で扱い、`now` からさかのぼった長さから求める。
    /// `Instant` から窓の長さを引くと、単調時計の起点の近く（起動直後）では表せない時刻になる
    /// 環境があるため。整数で数えるので、列の境目にちょうど接する区間が丸め誤差で隣の列へ
    /// 混ざることもない。
    fn for_each_overlap(
        &self,
        now: Instant,
        columns: usize,
        mut visit: impl FnMut(usize, f64, u128),
    ) {
        let window = self.window.as_nanos();
        if columns == 0 || window == 0 {
            return;
        }
        let count = columns as u128;
        // 列 c が受け持つのは [boundary(c), boundary(c + 1))。floor(window × c / columns) を、
        // 掛け算があふれない形に分けて求める。どの幅でも boundary(0) = 0、boundary(columns) = window。
        let boundary = |column: usize| {
            let column = column as u128;
            (window / count) * column + (window % count) * column / count
        };
        // 点は時刻の昇順で区間も重ならないので、重なり始める列は前の点より左へ戻らない。
        let mut first_column = 0;
        for point in &self.points {
            let span = point.span.as_nanos();
            // now からさかのぼった区間の終わりと始まり。now より後の部分は切り捨てる。
            let (end_age, start_age) = match now.checked_duration_since(point.end) {
                Some(behind) => (behind.as_nanos(), behind.as_nanos().saturating_add(span)),
                None => (
                    0,
                    span.saturating_sub(point.end.duration_since(now).as_nanos()),
                ),
            };
            // 窓の始まりから測った位置 [from, to)。窓より前の部分は切り捨てる。
            let from = window.saturating_sub(start_age);
            let to = window.saturating_sub(end_age);
            if from >= to {
                continue;
            }
            while first_column + 1 < columns && boundary(first_column + 1) <= from {
                first_column += 1;
            }
            let mut column = first_column;
            while column < columns && boundary(column) < to {
                let overlap = boundary(column + 1)
                    .min(to)
                    .saturating_sub(boundary(column).max(from));
                if overlap > 0 {
                    visit(column, point.value, overlap);
                }
                column += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(300);
    const INTERVAL: Duration = Duration::from_secs(1);

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    /// `times` の各秒に `value(t)` を記録した、窓 300 秒・間隔 1 秒の系列。
    fn series_from(
        base: Instant,
        times: impl IntoIterator<Item = u64>,
        value: impl Fn(u64) -> f64,
    ) -> Series {
        let mut series = Series::new(WINDOW, INTERVAL);
        for t in times {
            series.push(at(base, t), value(t));
        }
        series
    }

    fn assert_columns(actual: &[Option<f64>], expected: &[Option<f64>]) {
        assert_eq!(actual.len(), expected.len(), "{actual:?}");
        for (index, (actual_value, expected_value)) in actual.iter().zip(expected).enumerate() {
            match (actual_value, expected_value) {
                (Some(a), Some(e)) => {
                    assert!(
                        (a - e).abs() < 1e-6,
                        "column {index}: {a} != {e} in {actual:?}"
                    )
                }
                (None, None) => {}
                _ => panic!("column {index}: {actual_value:?} != {expected_value:?} in {actual:?}"),
            }
        }
    }

    fn repeated(value: Option<f64>, count: usize) -> Vec<Option<f64>> {
        vec![value; count]
    }

    #[test]
    fn constant_values_give_the_same_value_in_every_column() {
        let base = Instant::now();
        let series = series_from(base, 1..=600, |_| 42.0);
        let now = at(base, 600);
        assert_columns(&series.columns_mean(now, 60), &repeated(Some(42.0), 60));
        assert_columns(&series.columns_max(now, 60), &repeated(Some(42.0), 60));
        assert_eq!(series.latest(), Some(42.0));
    }

    #[test]
    fn step_change_lands_in_the_column_for_its_time() {
        // t ∈ (0, 150] は 0、(150, 300] は 100。窓 [0, 300] を 60 列（5 秒ずつ）に分ける。
        let base = Instant::now();
        let series = series_from(base, 1..=300, |t| if t <= 150 { 0.0 } else { 100.0 });
        let mut expected = repeated(Some(0.0), 30);
        expected.extend(repeated(Some(100.0), 30));
        assert_columns(&series.columns_mean(at(base, 300), 60), &expected);
    }

    #[test]
    fn column_straddling_a_step_is_weighted_by_overlap_time() {
        // 7 列だと 1 列は 300/7 ≈ 42.86 秒。列 3 は [128.57, 171.43) で、150 秒の段差の前後に
        // 同じ長さずつ重なるので平均は 50。
        let base = Instant::now();
        let series = series_from(base, 1..=300, |t| if t <= 150 { 0.0 } else { 100.0 });
        assert_columns(
            &series.columns_mean(at(base, 300), 7),
            &[
                Some(0.0),
                Some(0.0),
                Some(0.0),
                Some(50.0),
                Some(100.0),
                Some(100.0),
                Some(100.0),
            ],
        );
    }

    #[test]
    fn columns_before_the_first_point_are_none() {
        // 窓 300 秒のうち、最後の 60 秒（t ∈ (240, 300]）しか記録が無い。
        let base = Instant::now();
        let series = series_from(base, 241..=300, |_| 10.0);
        let now = at(base, 300);
        let mut expected = repeated(None, 48);
        expected.extend(repeated(Some(10.0), 12));
        assert_columns(&series.columns_mean(now, 60), &expected);
        assert_columns(&series.columns_max(now, 60), &expected);
    }

    #[test]
    fn long_gap_is_missing_and_the_next_point_covers_only_one_interval() {
        // t ∈ [1, 100] と [201, 300] に記録があり、間の 100 秒は止まっていた（スリープなど）。
        // t=201 の値で (100, 200] を埋めてはいけない。
        let base = Instant::now();
        let series = series_from(base, (1..=100).chain(201..=300), |t| {
            if t <= 100 { 20.0 } else { 80.0 }
        });
        let now = at(base, 300);
        let mut expected = repeated(Some(20.0), 20);
        expected.extend(repeated(None, 20));
        expected.extend(repeated(Some(80.0), 20));
        assert_columns(&series.columns_mean(now, 60), &expected);
        assert_columns(&series.columns_max(now, 60), &expected);
    }

    #[test]
    fn gap_up_to_three_intervals_is_treated_as_continuous() {
        // 窓 12 秒を 3 秒ずつの 4 列に分ける。
        // t=3: 最初の点なので (2, 3]。t=6: 間隔 3 秒（ちょうど 3 倍）は連続とみなし (3, 6]。
        // t=10: 間隔 4 秒は欠測なので (9, 10] だけ。t=12: (10, 12]。
        let base = Instant::now();
        let mut series = Series::new(Duration::from_secs(12), INTERVAL);
        for (t, value) in [(3, 1.0), (6, 2.0), (10, 4.0), (12, 7.0)] {
            series.push(at(base, t), value);
        }
        // 列 3 = [9, 12) は (9, 10] の 4 と (10, 12] の 7 の時間重み付き平均 (4×1 + 7×2) / 3 = 6。
        assert_columns(
            &series.columns_mean(at(base, 12), 4),
            &[Some(1.0), Some(2.0), None, Some(6.0)],
        );
    }

    #[test]
    fn points_older_than_the_window_are_discarded() {
        let base = Instant::now();
        let series = series_from(base, 1..=1000, |t| t as f64);
        // 最後の点 t=1000 から見て、区間が 700 秒より前に終わった点（t < 700）は捨てる。
        assert_eq!(series.points.len(), 301);
        assert_eq!(
            series.points.front().map(|point| point.end),
            Some(at(base, 700))
        );
        // 窓 [700, 1000] の列 0 = [700, 705) は t=701〜705 の平均で、捨てた点も t=700 も混ざらない。
        assert_columns(
            &series.columns_mean(at(base, 1000), 60)[..1],
            &[Some(703.0)],
        );
    }

    #[test]
    fn different_widths_cover_the_same_time_range() {
        // 記録は最後の 60 秒だけ。どの幅でも、記録のある列は窓の右 1/5 に揃う。
        let base = Instant::now();
        let series = series_from(base, 241..=300, |_| 5.0);
        let now = at(base, 300);
        for columns in [10, 30, 60, 90] {
            let means = series.columns_mean(now, columns);
            assert_eq!(means.len(), columns);
            let first_with_data = means.iter().position(Option::is_some);
            assert_eq!(first_with_data, Some(columns * 4 / 5), "columns={columns}");
            assert_columns(&means[columns * 4 / 5..], &repeated(Some(5.0), columns / 5));
        }

        // 30 列の 1 列は 60 列の隣り合う 2 列と同じ時間を表す。
        let stepped = series_from(base, 1..=300, |t| (t / 7) as f64);
        let narrow = stepped.columns_mean(now, 30);
        let wide = stepped.columns_mean(now, 60);
        let paired: Vec<Option<f64>> = wide
            .chunks(2)
            .map(|pair| Some((pair[0]? + pair[1]?) / 2.0))
            .collect();
        assert_columns(&narrow, &paired);
    }

    #[test]
    fn zero_columns_is_empty_and_one_column_is_the_mean_of_the_window() {
        // (0, 100] は 10、(100, 300] は 40。窓全体の時間重み付き平均は (10×100 + 40×200) / 300 = 30。
        let base = Instant::now();
        let series = series_from(base, 1..=300, |t| if t <= 100 { 10.0 } else { 40.0 });
        let now = at(base, 300);
        assert!(series.columns_mean(now, 0).is_empty());
        assert!(series.columns_max(now, 0).is_empty());
        assert_columns(&series.columns_mean(now, 1), &[Some(30.0)]);
        assert_columns(&series.columns_max(now, 1), &[Some(40.0)]);
    }

    #[test]
    fn duplicate_and_backward_times_are_ignored() {
        let base = Instant::now();
        let mut series = Series::new(WINDOW, INTERVAL);
        assert_eq!(series.latest(), None);
        series.push(at(base, 10), 1.0);
        series.push(at(base, 10), 2.0);
        series.push(at(base, 5), 3.0);
        assert_eq!(series.latest(), Some(1.0));
        assert_eq!(series.points.len(), 1);
        // 次の点は、捨てた点ではなく t=10 からの区間を代表する。
        series.push(at(base, 11), 4.0);
        assert_eq!(series.points.back().map(|point| point.span), Some(INTERVAL));
    }

    #[test]
    fn max_keeps_short_spikes_that_the_mean_flattens() {
        // 逼迫度のような段階の値は、列の中で 1 秒でも上の段階になったらそれを見せたい。
        let base = Instant::now();
        let series = series_from(base, 1..=300, |t| if t == 153 { 4.0 } else { 1.0 });
        let now = at(base, 300);
        // t=153 の区間 (152, 153] は列 30 = [150, 155) に入る。平均は (1×4 + 4×1) / 5 = 1.6。
        let mut expected_max = repeated(Some(1.0), 60);
        expected_max[30] = Some(4.0);
        assert_columns(&series.columns_max(now, 60), &expected_max);
        assert_columns(&series.columns_mean(now, 60)[30..31], &[Some(1.6)]);
    }

    #[test]
    fn interval_ending_on_a_column_boundary_stays_in_its_column() {
        // t=5 の点は (4, 5] で、列 0 = [0, 5) の終わりにちょうど接する。列 1 には混ざらない。
        let base = Instant::now();
        let mut series = Series::new(Duration::from_secs(10), INTERVAL);
        series.push(at(base, 5), 9.0);
        assert_columns(&series.columns_mean(at(base, 10), 2), &[Some(9.0), None]);
    }

    #[test]
    fn columns_after_the_last_point_are_none() {
        // 計測が t=200 で止まり、描画だけが t=300 まで進んだ。
        let base = Instant::now();
        let series = series_from(base, 1..=200, |_| 7.0);
        let mut expected = repeated(Some(7.0), 40);
        expected.extend(repeated(None, 20));
        assert_columns(&series.columns_mean(at(base, 300), 60), &expected);
    }

    #[test]
    fn points_after_now_are_not_counted() {
        // 最後の点より前の時刻で描いても、その時刻より後の区間は数えない。
        let base = Instant::now();
        let series = series_from(base, 1..=300, |t| if t <= 150 { 1.0 } else { 9.0 });
        let mut expected = repeated(None, 30);
        expected.extend(repeated(Some(1.0), 30));
        assert_columns(&series.columns_mean(at(base, 150), 60), &expected);
    }

    #[test]
    fn mean_of_huge_finite_values_does_not_overflow() {
        // 値 × 重なり時間 [ns] の総和を取る作りだと、1e300 でも無限大になる。
        let base = Instant::now();
        let series = series_from(base, 1..=300, |t| if t <= 150 { 1e300 } else { 3e300 });
        let mean = series.columns_mean(at(base, 300), 1)[0].expect("data covers the window");
        assert!(((mean - 2e300) / 2e300).abs() < 1e-9, "{mean}");
    }

    #[test]
    fn non_finite_values_are_ignored() {
        let base = Instant::now();
        let mut series = Series::new(Duration::from_secs(4), INTERVAL);
        series.push(at(base, 1), 1.0);
        series.push(at(base, 2), f64::NAN);
        series.push(at(base, 3), f64::INFINITY);
        series.push(at(base, 4), 3.0);
        assert_eq!(series.latest(), Some(3.0));
        // 捨てた 2 点は無かったことになり、t=4 の値が (1, 4] を代表する。
        assert_columns(
            &series.columns_mean(at(base, 4), 4),
            &[Some(1.0), Some(3.0), Some(3.0), Some(3.0)],
        );
    }
}

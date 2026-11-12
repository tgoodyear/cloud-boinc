//! Charts as inline SVG, built on the server. No script, no external request,
//! no inline `style` attribute: the colours come from the classes in the site
//! stylesheet (`.chart`, `.ch-*`), which use the page's light and dark
//! variables.
//!
//! The drawing is 400 units wide and scaled to the container, so on a phone
//! (about 350 px) it is nearly 1:1 and the 12-unit text stays readable; the
//! stylesheet caps the width on a wide screen. Every chart is paired with a
//! table of the same numbers (`data_details`) for screen readers and for
//! anyone who wants the figures.
//!
//! A value that is not known is `None`: it is left out of the drawing, and
//! the table says "unknown". It is never drawn as zero.

use crate::site::civil_from_days;
use crate::web::html_escape;

const WIDTH: f64 = 400.0;
const HEIGHT: f64 = 176.0;
const LEFT: f64 = 46.0;
const RIGHT: f64 = 8.0;
const TOP: f64 = 10.0;
const BOTTOM: f64 = 26.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Bars,
    Area,
    Lines,
}

/// One series: a label for the legend and one value per time slot.
pub struct Series<'a> {
    pub label: &'a str,
    pub values: &'a [Option<f64>],
    /// Per slot: the value is a lower bound, drawn as it is and shown with a
    /// `+` in the table. Empty when none is.
    pub at_least: &'a [bool],
}

/// Where the x axis is labelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    /// At each UTC midnight, with the weekday: for periods shorter than a day.
    Midnights,
    /// At every few slots, with the date: for a day or longer.
    Dates,
}

pub struct Chart<'a> {
    pub title: &'a str,
    /// What the chart shows, one sentence, for the accessible description.
    pub description: &'a str,
    pub kind: Kind,
    pub axis: Axis,
    /// Start of each slot, Unix seconds, increasing.
    pub times: &'a [i64],
    pub series: &'a [Series<'a>],
    /// The last slot is still filling: it is drawn fainter.
    pub last_partial: bool,
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `Mon 9`, the label of a midnight.
fn day_label(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let (_, _, d) = civil_from_days(days);
    format!("{} {d}", WEEKDAYS[days.rem_euclid(7) as usize])
}

/// `9 Nov`, the label of a date.
fn short_date(t: i64) -> String {
    let (_, m, d) = civil_from_days(t.div_euclid(86_400));
    format!("{d} {}", MONTHS[(m - 1) as usize])
}

/// `9 Nov, 15:00`, a slot start for the table.
pub fn slot_label(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let (_, m, d) = civil_from_days(days);
    let secs = t.rem_euclid(86_400);
    format!(
        "{d} {}, {:02}:{:02}",
        MONTHS[(m - 1) as usize],
        secs / 3600,
        secs % 3600 / 60
    )
}

/// 1, 2 or 5 times a power of ten that is at least `max`.
pub fn nice_top(max: f64) -> f64 {
    if !max.is_finite() || max <= 0.0 {
        return 1.0;
    }
    let p = 10f64.powf(max.log10().floor());
    for m in [1.0, 2.0, 5.0, 10.0] {
        if m * p >= max * (1.0 - 1e-9) {
            return m * p;
        }
    }
    10.0 * p
}

/// `1,234`, `12.5k`, `3.2M`, or up to two decimals for a small fraction.
pub fn compact(v: f64) -> String {
    let a = v.abs();
    let trim = |s: String| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if a >= 1e6 {
        format!("{}M", trim(format!("{:.1}", v / 1e6)))
    } else if a >= 1e4 {
        format!("{}k", trim(format!("{:.1}", v / 1e3)))
    } else if a >= 100.0 || v.fract() == 0.0 {
        crate::site::number_format(v, 0)
    } else {
        trim(format!("{v:.2}"))
    }
}

impl Chart<'_> {
    fn n(&self) -> usize {
        self.times.len()
    }

    fn plot_w(&self) -> f64 {
        WIDTH - LEFT - RIGHT
    }

    fn plot_h(&self) -> f64 {
        HEIGHT - TOP - BOTTOM
    }

    fn pitch(&self) -> f64 {
        self.plot_w() / self.n().max(1) as f64
    }

    fn x_mid(&self, i: usize) -> f64 {
        LEFT + self.pitch() * (i as f64 + 0.5)
    }

    fn y(&self, v: f64, top: f64) -> f64 {
        TOP + self.plot_h() * (1.0 - (v / top).clamp(0.0, 1.0))
    }

    fn max(&self) -> f64 {
        self.series
            .iter()
            .flat_map(|s| s.values.iter().flatten())
            .copied()
            .fold(0.0, f64::max)
    }

    fn whole_numbers(&self) -> bool {
        self.series
            .iter()
            .flat_map(|s| s.values.iter().flatten())
            .all(|v| v.fract() == 0.0)
    }

    /// The SVG element. `id` makes the `<title>` and `<desc>` ids unique on
    /// a page that has several charts.
    pub fn svg(&self, id: &str) -> String {
        let top = nice_top(self.max());
        let mut o = format!(
            "<svg class=\"chart\" viewBox=\"0 0 {WIDTH} {HEIGHT}\" role=\"img\" \
             aria-labelledby=\"{id}-t {id}-d\">\n<title id=\"{id}-t\">{}</title>\n\
             <desc id=\"{id}-d\">{}</desc>\n",
            html_escape(self.title),
            html_escape(self.description)
        );
        // Gridlines and y labels.
        let mut ticks = vec![0.0, top];
        if !self.whole_numbers() || (top >= 2.0 && top.fract() == 0.0 && (top as i64) % 2 == 0) {
            ticks.insert(1, top / 2.0);
        }
        for t in ticks {
            let y = self.y(t, top);
            o.push_str(&format!(
                "<line class=\"{}\" x1=\"{LEFT}\" x2=\"{}\" y1=\"{y:.1}\" y2=\"{y:.1}\"/>\
                 <text x=\"{}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>\n",
                if t == 0.0 { "ch-axis" } else { "ch-grid" },
                WIDTH - RIGHT,
                LEFT - 5.0,
                y + 4.0,
                compact(t)
            ));
        }
        // Ticks and labels along the bottom.
        let every = match self.axis {
            Axis::Midnights => 1,
            Axis::Dates => (52.0 / self.pitch()).ceil().max(1.0) as usize,
        };
        for (i, &t) in self.times.iter().enumerate() {
            let (show, label) = match self.axis {
                Axis::Midnights => (t.rem_euclid(86_400) == 0, day_label(t)),
                Axis::Dates => (i % every == 0, short_date(t)),
            };
            let x = LEFT + self.pitch() * i as f64;
            // A label that would run off the right edge is left out.
            if !show || x + 2.0 + 6.5 * label.len() as f64 > WIDTH {
                continue;
            }
            o.push_str(&format!(
                "<line class=\"ch-grid\" x1=\"{x:.1}\" x2=\"{x:.1}\" y1=\"{TOP}\" y2=\"{}\"/>\
                 <text x=\"{:.1}\" y=\"{:.1}\">{label}</text>\n",
                HEIGHT - BOTTOM,
                x + 2.0,
                HEIGHT - 8.0,
            ));
        }
        for (si, s) in self.series.iter().enumerate() {
            match self.kind {
                Kind::Bars => self.bars(&mut o, s, si, top),
                Kind::Area => {
                    self.area(&mut o, s, si, top);
                    self.line(&mut o, s, si, top);
                }
                Kind::Lines => self.line(&mut o, s, si, top),
            }
        }
        o.push_str("</svg>\n");
        o
    }

    fn bars(&self, o: &mut String, s: &Series, si: usize, top: f64) {
        let w = (self.pitch() - 1.4).max(1.0);
        for (i, v) in s.values.iter().enumerate().take(self.n()) {
            let Some(v) = v else { continue };
            if *v <= 0.0 {
                continue;
            }
            let y = self.y(*v, top);
            let partial = self.last_partial && i + 1 == self.n();
            o.push_str(&format!(
                "<rect class=\"ch-bar ch-s{si}{}\" x=\"{:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{:.1}\"/>\n",
                if partial { " ch-partial" } else { "" },
                self.x_mid(i) - w / 2.0,
                (TOP + self.plot_h() - y).max(0.5)
            ));
        }
    }

    /// Runs of consecutive known values.
    fn runs(&self, s: &Series) -> Vec<Vec<(usize, f64)>> {
        let mut runs: Vec<Vec<(usize, f64)>> = Vec::new();
        let mut cur: Vec<(usize, f64)> = Vec::new();
        for (i, v) in s.values.iter().enumerate().take(self.n()) {
            match v {
                Some(v) => cur.push((i, *v)),
                None => {
                    if !cur.is_empty() {
                        runs.push(std::mem::take(&mut cur));
                    }
                }
            }
        }
        if !cur.is_empty() {
            runs.push(cur);
        }
        runs
    }

    fn line(&self, o: &mut String, s: &Series, si: usize, top: f64) {
        for run in self.runs(s) {
            if run.len() == 1 {
                let (i, v) = run[0];
                o.push_str(&format!(
                    "<circle class=\"ch-dot ch-s{si}\" cx=\"{:.1}\" cy=\"{:.1}\" r=\"2.5\"/>\n",
                    self.x_mid(i),
                    self.y(v, top)
                ));
                continue;
            }
            let d: String = run
                .iter()
                .enumerate()
                .map(|(k, (i, v))| {
                    format!(
                        "{}{:.1},{:.1}",
                        if k == 0 { 'M' } else { 'L' },
                        self.x_mid(*i),
                        self.y(*v, top)
                    )
                })
                .collect();
            o.push_str(&format!(
                "<path class=\"ch-line ch-s{si}\" d=\"{d}\"{}/>\n",
                if si > 0 {
                    " stroke-dasharray=\"5 3\""
                } else {
                    ""
                }
            ));
        }
    }

    fn area(&self, o: &mut String, s: &Series, si: usize, top: f64) {
        let base = TOP + self.plot_h();
        for run in self.runs(s) {
            if run.len() < 2 {
                continue;
            }
            let mut d = format!("M{:.1},{base:.1}", self.x_mid(run[0].0));
            for (i, v) in &run {
                d.push_str(&format!("L{:.1},{:.1}", self.x_mid(*i), self.y(*v, top)));
            }
            d.push_str(&format!(
                "L{:.1},{base:.1}Z",
                self.x_mid(run[run.len() - 1].0)
            ));
            o.push_str(&format!("<path class=\"ch-area ch-s{si}\" d=\"{d}\"/>\n"));
        }
    }

    /// The legend: one entry per series, drawn with the series' own colour
    /// and dash, as HTML under the chart.
    pub fn legend(&self) -> String {
        let mut o = String::from("<ul class=\"legend\">");
        for (si, s) in self.series.iter().enumerate() {
            o.push_str(&format!(
                "<li><svg class=\"legend-key\" viewBox=\"0 0 18 10\" aria-hidden=\"true\"><line class=\"ch-line ch-s{si}\" x1=\"1\" x2=\"17\" y1=\"5\" y2=\"5\"{}/></svg>{}</li>",
                if si > 0 { " stroke-dasharray=\"5 3\"" } else { "" },
                html_escape(s.label)
            ));
        }
        o.push_str("</ul>\n");
        o
    }

    /// The same numbers as a table inside `<details>`, a row per slot.
    pub fn data_details(&self, unit_note: &str) -> String {
        let mut o = String::from(
            "<details><summary>Table of the values</summary>\n<table>\n<tr><th>Period starting (UTC)</th>",
        );
        for s in self.series {
            o.push_str(&format!("<th class=\"num\">{}</th>", html_escape(s.label)));
        }
        o.push_str("</tr>\n");
        for (i, &t) in self.times.iter().enumerate().rev() {
            o.push_str(&format!("<tr><td>{}</td>", slot_label(t)));
            for s in self.series {
                match s.values.get(i).copied().flatten() {
                    Some(v) => o.push_str(&format!(
                        "<td class=\"num\">{}{}</td>",
                        compact_full(v),
                        if s.at_least.get(i) == Some(&true) {
                            "+"
                        } else {
                            ""
                        }
                    )),
                    None => o.push_str("<td class=\"num muted\">unknown</td>"),
                }
            }
            o.push_str("</tr>\n");
        }
        o.push_str("</table>\n");
        if !unit_note.is_empty() {
            o.push_str(&format!(
                "<p><small>{}</small></p>\n",
                html_escape(unit_note)
            ));
        }
        o.push_str("</details>\n");
        o
    }
}

/// A value in the table: whole numbers with separators, others to two places.
fn compact_full(v: f64) -> String {
    if v.fract() == 0.0 {
        crate::site::number_format(v, 0)
    } else {
        crate::site::number_format(v, 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_tops_and_labels() {
        assert_eq!(nice_top(0.0), 1.0);
        assert_eq!(nice_top(3.0), 5.0);
        assert_eq!(nice_top(7.0), 10.0);
        assert_eq!(nice_top(10.0), 10.0);
        assert_eq!(nice_top(130.0), 200.0);
        assert!((nice_top(0.93) - 1.0).abs() < 1e-9);
        assert_eq!(compact(0.0), "0");
        assert_eq!(compact(250.0), "250");
        assert_eq!(compact(12_500.0), "12.5k");
        assert_eq!(compact(3_200_000.0), "3.2M");
        assert_eq!(compact(0.5), "0.5");
    }

    #[test]
    fn day_labels() {
        // 2000-01-01 was a Saturday.
        assert_eq!(day_label(946_684_800), "Sat 1");
        assert_eq!(slot_label(946_684_800 + 15 * 3600), "1 Jan, 15:00");
    }

    #[test]
    fn unknown_slots_leave_gaps_and_are_not_zero() {
        let times: Vec<i64> = (0..6).map(|i| 946_684_800 + i * 10_800).collect();
        let values = [Some(4.0), None, None, Some(2.0), Some(3.0), None];
        let series = [Series {
            label: "Tasks",
            values: &values,
            at_least: &[],
        }];
        let chart = Chart {
            title: "Tasks",
            description: "Tasks per period",
            kind: Kind::Lines,
            axis: Axis::Midnights,
            times: &times,
            series: &series,
            last_partial: false,
        };
        let svg = chart.svg("c1");
        // One lone point and one two-point run; no segment crosses a gap.
        assert_eq!(svg.matches("<circle").count(), 1);
        assert_eq!(svg.matches("<path").count(), 1);
        assert!(svg.contains("role=\"img\""));
        let table = chart.data_details("");
        assert_eq!(table.matches("unknown").count(), 3);
    }

    #[test]
    fn bars_and_escaping() {
        let times: Vec<i64> = (0..4).map(|i| 946_684_800 + i * 10_800).collect();
        let values = [Some(0.0), Some(5.0), None, Some(10.0)];
        let series = [Series {
            label: "A <b>",
            values: &values,
            at_least: &[],
        }];
        let chart = Chart {
            title: "T <script>",
            description: "d",
            kind: Kind::Bars,
            axis: Axis::Midnights,
            times: &times,
            series: &series,
            last_partial: true,
        };
        let svg = chart.svg("c2");
        assert_eq!(svg.matches("<rect").count(), 2);
        assert!(svg.contains("ch-partial"));
        assert!(!svg.contains("<script>"));
        assert!(chart.legend().contains("A &lt;b&gt;"));
        assert!(!svg.contains("style="));
    }
}

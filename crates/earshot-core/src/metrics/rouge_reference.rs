//! Reference cases for [`super::rouge`] against Google's `rouge-score` package.
//!
//! Every expected value below is the output of upstream `rouge_scorer.RougeScorer(["rouge1",
//! "rouge2", "rougeL", "rougeLsum"])` (google-research/google-research `rouge/` at commit
//! 0b7b841b4d58aac0854fd6daa3f80ea2724d14b3, default `use_stemmer=False`,
//! `split_summaries=False`), run once under CPython 3.14 outside the build and printed with
//! `repr`. The scoring code ran unchanged; only its third-party imports that these options
//! never call (absl logging, numpy, six's Python 2 shims, nltk's stemmer and sentence
//! splitter) were stubbed. They were not produced by this crate; new cases must be produced
//! the same way. Values are compared exactly: both sides divide the same integers in the same
//! order.

use super::rouge::{RougeScore, rouge};

/// `[precision, recall, fmeasure]` for rouge1, rouge2, rougeL, rougeLsum.
#[track_caller]
fn case(target: &str, prediction: &str, upstream: [[f64; 3]; 4]) {
    let s = rouge(target, prediction);
    let got = [s.rouge1, s.rouge2, s.rouge_l, s.rouge_lsum];
    for (name, (g, u)) in ["rouge1", "rouge2", "rougeL", "rougeLsum"]
        .iter()
        .zip(got.iter().zip(upstream))
    {
        let want = RougeScore {
            precision: u[0],
            recall: u[1],
            fmeasure: u[2],
        };
        assert_eq!(
            *g, want,
            "{name} of target {target:?} / prediction {prediction:?}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn rouge_matches_upstream_rouge_score() {
    case(
        "the cat sat on the mat",
        "the cat is on the mat",
        [
            [0.8333333333333334, 0.8333333333333334, 0.8333333333333334],
            [0.6, 0.6, 0.6],
            [0.8333333333333334, 0.8333333333333334, 0.8333333333333334],
            [0.8333333333333334, 0.8333333333333334, 0.8333333333333334],
        ],
    );
    case(
        "a b c d",
        "a c x d",
        [
            [0.75, 0.75, 0.75],
            [0.0, 0.0, 0.0],
            [0.75, 0.75, 0.75],
            [0.75, 0.75, 0.75],
        ],
    );
    case(
        "one two three",
        "one two",
        [
            [1.0, 0.6666666666666666, 0.8],
            [1.0, 0.5, 0.6666666666666666],
            [1.0, 0.6666666666666666, 0.8],
            [1.0, 0.6666666666666666, 0.8],
        ],
    );
    case(
        "",
        "something",
        [
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        ],
    );
    case(
        "something",
        "",
        [
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        ],
    );
    case(
        "-- ...",
        "a",
        [
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        ],
    );
    case(
        "A b C",
        "a B c",
        [
            [1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
        ],
    );
    case(
        "the the the a",
        "the a the",
        [
            [1.0, 0.75, 0.8571428571428571],
            [0.5, 0.3333333333333333, 0.4],
            [0.6666666666666666, 0.5, 0.5714285714285715],
            [0.6666666666666666, 0.5, 0.5714285714285715],
        ],
    );
    case(
        "w1 w2 w3 w4 w5\nw6 w7",
        "w1 w2 w6 w7 w8\nw3 w9 w4",
        [
            [0.75, 0.8571428571428571, 0.7999999999999999],
            [0.2857142857142857, 0.3333333333333333, 0.30769230769230765],
            [0.5, 0.5714285714285714, 0.5333333333333333],
            [0.75, 0.8571428571428571, 0.7999999999999999],
        ],
    );
    case(
        "x y z\n\ny x z\n",
        "z y x\nx z",
        [
            [1.0, 0.8333333333333334, 0.9090909090909091],
            [0.75, 0.6, 0.6666666666666665],
            [0.8, 0.6666666666666666, 0.7272727272727272],
            [1.0, 0.8333333333333334, 0.9090909090909091],
        ],
    );
    case(
        "a b a b a\nb a b",
        "b a b a\na a b b",
        [
            [1.0, 1.0, 1.0],
            [0.5714285714285714, 0.5714285714285714, 0.5714285714285714],
            [0.75, 0.75, 0.75],
            [1.0, 1.0, 1.0],
        ],
    );
    case(
        "Police killed the gunman.\nThe gunman was shot.",
        "The gunman police killed.\nPolice shot the gunman.",
        [
            [0.875, 0.875, 0.875],
            [
                0.42857142857142855,
                0.42857142857142855,
                0.42857142857142855,
            ],
            [0.5, 0.5, 0.5],
            [0.75, 0.75, 0.75],
        ],
    );
    case(
        "The LCD-screen's 3 buttons cost 12.50 euros, caf\u{e9} au lait!",
        "lcd screen buttons: 3; cost 12 50 EUROS cafe",
        [
            [0.8888888888888888, 0.6153846153846154, 0.7272727272727274],
            [0.5, 0.3333333333333333, 0.4],
            [0.7777777777777778, 0.5384615384615384, 0.6363636363636364],
            [0.7777777777777778, 0.5384615384615384, 0.6363636363636364],
        ],
    );
    case(
        "\u{c9}cole na\u{ef}ve r\u{e9}sum\u{e9} \u{212a}\u{130}",
        "cole na ve r sum k i",
        [
            [0.7142857142857143, 0.8333333333333334, 0.7692307692307692],
            [0.6666666666666666, 0.8, 0.7272727272727272],
            [0.7142857142857143, 0.8333333333333334, 0.7692307692307692],
            [0.7142857142857143, 0.8333333333333334, 0.7692307692307692],
        ],
    );
    case(
        "tabs\tand  spaces\r\nwindows lines",
        "tabs and spaces\nwindows\r\nlines",
        [
            [1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
        ],
    );
    case(
        "The Project Manager gave an introduction to the goal of the project, to create a trendy yet user-friendly remote.\nShe presented a long-range agenda for the whole project.\nThe group introduced themselves to each other and practiced with the meeting room tools by drawing on the board.\nThe Project Manager presented the project budget, the projected price point, and the projected profit aim for the project.",
        "The team kicked off the remote control design project and agreed on a trendy, user-friendly product.\nBudget and selling price: the remote should sell for 25 euros with a profit aim of 50 million euros.\nEveryone drew their favourite animal on the whiteboard to practise with the tools.\nThe project manager will send the minutes.",
        [
            [0.5, 0.4117647058823529, 0.45161290322580644],
            [0.12727272727272726, 0.1044776119402985, 0.11475409836065573],
            [0.32142857142857145, 0.2647058823529412, 0.2903225806451613],
            [0.4642857142857143, 0.38235294117647056, 0.41935483870967744],
        ],
    );
    case(
        "a a a a b\nb b a",
        "a b\nb a\na a\nb b",
        [
            [0.875, 0.875, 0.875],
            [0.8571428571428571, 0.8571428571428571, 0.8571428571428571],
            [0.75, 0.75, 0.75],
            [0.75, 0.75, 0.75],
        ],
    );
    case(
        "1 2 3 4 5 6 7 8 9 10",
        "10 9 8 7 6 5 4 3 2 1",
        [
            [1.0, 1.0, 1.0],
            [0.0, 0.0, 0.0],
            [0.1, 0.1, 0.10000000000000002],
            [0.1, 0.1, 0.10000000000000002],
        ],
    );
}

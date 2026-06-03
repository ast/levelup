//! `/proc/loadavg` — 1/5/15-minute load averages.

use std::path::Path;

use nom::{
    IResult, Parser, character::complete::space1, number::complete::double,
    sequence::separated_pair,
};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoadAvg {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// Parse the leading three floats of `/proc/loadavg` (the trailing
/// `runnable/total lastpid` fields are ignored).
fn loadavg(input: &str) -> IResult<&str, LoadAvg> {
    let (input, (one, (five, fifteen))) =
        separated_pair(double, space1, separated_pair(double, space1, double)).parse(input)?;
    Ok((input, LoadAvg { one, five, fifteen }))
}

pub fn parse(input: &str) -> Option<LoadAvg> {
    loadavg(input.trim_start()).ok().map(|(_, l)| l)
}

pub fn read() -> std::io::Result<Option<LoadAvg>> {
    Ok(parse(&std::fs::read_to_string(Path::new("/proc/loadavg"))?))
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_three_averages() {
        let l = super::parse("0.52 0.58 0.59 1/523 12345\n").unwrap();
        assert_eq!(l.one, 0.52);
        assert_eq!(l.five, 0.58);
        assert_eq!(l.fifteen, 0.59);
    }

    #[test]
    fn rejects_garbage() {
        assert!(super::parse("not a loadavg").is_none());
    }
}

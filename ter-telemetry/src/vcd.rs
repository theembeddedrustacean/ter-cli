//! Value Change Dump files, as a logic analyzer writes them.
//!
//! Only what a digital capture needs: one-bit wires, their names, and when
//! each changed. Times come back in nanoseconds whatever the file's
//! `$timescale`.

use std::collections::HashMap;

use crate::event::Level;

/// One change of one signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub t_ns: u64,
    /// The signal's name in the file (`D0`, ...).
    pub signal: String,
    /// `None` for `x` or `z`: the level was unknown.
    pub level: Option<Level>,
}

/// The changes in `text`, in file order, which is time order.
pub fn parse(text: &str) -> Result<Vec<Change>, String> {
    let mut tokens = text.split_whitespace().peekable();
    let mut ps_per_tick: u64 = 1_000;
    let mut names: HashMap<String, String> = HashMap::new();
    let mut now: u64 = 0;
    let mut out = Vec::new();

    while let Some(tok) = tokens.next() {
        match tok {
            "$timescale" => {
                let mut spec = String::new();
                for t in tokens.by_ref() {
                    if t == "$end" {
                        break;
                    }
                    spec.push_str(t);
                }
                ps_per_tick = timescale_ps(&spec)?;
            }
            "$var" => {
                // $var <type> <size> <id> <reference> [range] $end
                let fields: Vec<&str> = tokens.by_ref().take_while(|t| *t != "$end").collect();
                if fields.len() < 4 {
                    return Err(format!("short $var declaration: {}", fields.join(" ")));
                }
                if fields[1] == "1" {
                    names.insert(fields[2].to_string(), fields[3].to_string());
                }
            }
            // Sections whose body is not values.
            "$comment" | "$date" | "$version" | "$scope" | "$upscope" | "$enddefinitions" => {
                for t in tokens.by_ref() {
                    if t == "$end" {
                        break;
                    }
                }
            }
            // Value sections: their values are read as ordinary changes.
            "$dumpvars" | "$dumpall" | "$dumpon" | "$dumpoff" | "$end" => {}
            t if t.starts_with('#') => {
                let ticks: u64 = t[1..]
                    .parse()
                    .map_err(|_| format!("bad time stamp {t:?}"))?;
                now = ticks
                    .checked_mul(ps_per_tick)
                    .ok_or_else(|| format!("time stamp {t} overflows"))?
                    / 1_000;
            }
            t if t.starts_with(['b', 'B', 'r', 'R']) => {
                // A vector or a real: its identifier follows. Not a wire.
                tokens.next();
            }
            t => {
                let (value, id) = t.split_at(1);
                let level = match value {
                    "0" => Some(Level::Low),
                    "1" => Some(Level::High),
                    "x" | "X" | "z" | "Z" => None,
                    _ => return Err(format!("unexpected token {t:?}")),
                };
                if let Some(signal) = names.get(id) {
                    out.push(Change {
                        t_ns: now,
                        signal: signal.clone(),
                        level,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// `1ns`, `10 us`, `100ps` as picoseconds per tick.
fn timescale_ps(spec: &str) -> Result<u64, String> {
    let digits: String = spec.chars().take_while(char::is_ascii_digit).collect();
    let unit = &spec[digits.len()..];
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("bad $timescale {spec:?}"))?;
    let scale = match unit {
        "s" => 1_000_000_000_000,
        "ms" => 1_000_000_000,
        "us" => 1_000_000,
        "ns" => 1_000,
        "ps" => 1,
        _ => return Err(format!("unsupported $timescale {spec:?}")),
    };
    Ok(n * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
$date today $end
$version wokwi $end
$timescale 1ns $end
$scope module logic $end
$var wire 1 ! D0 $end
$var wire 1 \" D1 $end
$var wire 8 # bus $end
$upscope $end
$enddefinitions $end
#0
$dumpvars
0!
x\"
b00000000 #
$end
#1500000
1!
#2000000
0!
1\"
";

    #[test]
    fn wires_changes_and_times_in_nanoseconds() {
        let changes = parse(SAMPLE).unwrap();
        let c = |t_ns, signal: &str, level| Change {
            t_ns,
            signal: signal.into(),
            level,
        };
        assert_eq!(
            changes,
            vec![
                c(0, "D0", Some(Level::Low)),
                c(0, "D1", None),
                c(1_500_000, "D0", Some(Level::High)),
                c(2_000_000, "D0", Some(Level::Low)),
                c(2_000_000, "D1", Some(Level::High)),
            ]
        );
    }

    #[test]
    fn timescale_is_applied() {
        let text = "$timescale 10 us $end $var wire 1 a D0 $end $enddefinitions $end #3 1a";
        assert_eq!(parse(text).unwrap()[0].t_ns, 30_000);
        let ps = "$timescale 100ps $end $var wire 1 a D0 $end $enddefinitions $end #25 1a";
        assert_eq!(parse(ps).unwrap()[0].t_ns, 2);
        assert!(parse("$timescale 1fs $end").is_err());
    }
}

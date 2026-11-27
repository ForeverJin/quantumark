// 调度器：每 30 秒检查一次定时任务。
//
// 语义：
//   - 每个任务一条 5 段 cron（内部补上秒位），时区跟随 TZ。
//   - 启动/改表达式时，如果“停机补跑窗口”内错过了一次，补跑一次；窗口由策略配置
//     （misfire_window_s，默认 24 小时，设为 0 表示不补跑）。
//   - 同一任务不会重叠（交给 Runner 的 active 集合判断）。
//   - 风控冷却期内跳过定时触发（Runner 里再判一次）。

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use cron::Schedule;

use crate::runner::{Runner, get_cooldown};
use crate::store::{Store, now};

const TICK: Duration = Duration::from_secs(30);

struct JobSchedule {
    expr: String,
    schedule: Schedule,
    next_due: DateTime<Local>,
}

pub fn spawn(runner: Runner, store: Arc<Store>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut states: HashMap<i64, JobSchedule> = HashMap::new();
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            let jobs = match store.list_jobs() {
                Ok(j) => j,
                Err(e) => {
                    tracing::error!("读取任务列表失败: {e}");
                    continue;
                }
            };
            let now_local = Local::now();
            // 补跑窗口可配置；设成 0 表示重启/改表达式后不自动补跑
            let misfire = chrono::TimeDelta::seconds(
                store.policy().misfire_window_s.min(31_536_000) as i64,
            );

            // 清理已删除/已停用任务的调度状态
            let alive: Vec<i64> = jobs.iter().map(|j| j.id).collect();
            states.retain(|id, _| alive.contains(id));

            for job in jobs {
                let Some(expr) = job.cron.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
                    continue;
                };
                if !job.enabled {
                    continue;
                }
                let schedule = match parse(expr) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(job.id, "cron 表达式无效（{expr}）：{e}");
                        continue;
                    }
                };

                let state = match states.get_mut(&job.id) {
                    Some(s) if s.expr == expr => s,
                    _ => {
                        // 首次见到或表达式变了：确定下一次触发时间
                        let next_due = initial_due(&schedule, now_local, misfire);
                        states.insert(
                            job.id,
                            JobSchedule {
                                expr: expr.to_string(),
                                schedule,
                                next_due,
                            },
                        );
                        states.get_mut(&job.id).expect("刚插入")
                    }
                };

                if now_local < state.next_due {
                    continue;
                }

                // 冷却期内不自动跑
                if let Some(until) = get_cooldown(&store) {
                    if until > now() {
                        tracing::warn!(job.id, "处于风控冷却期，跳过定时任务");
                        state.next_due = next_after(&state.schedule, now_local);
                        continue;
                    }
                }

                match runner.enqueue(job.id, "schedule").await {
                    Ok(()) => {}
                    Err(e) => tracing::info!(job.id, "跳过定时触发: {e}"),
                }
                // 从「现在」往后推，避免一次补跑多个错过的时间点
                state.next_due = next_after(&state.schedule, now_local);
            }
        }
    })
}

/// 解析定时表达式。
///
/// 对外统一用**标准 5 段 cron**（分 时 日 月 周），星期编号 0/7=周日、1=周一…6=周六。
/// `cron` crate 用的是 Quartz 风格（1=周日…7=周六），所以这里要换算，否则"每周一"会跑到周日。
/// 6/7 段视为高级用法，原样交给 crate（按 crate 的编号）。
pub fn parse(expr: &str) -> Result<Schedule, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let normalized = match fields.len() {
        5 => {
            let dow = convert_dow(fields[4])?;
            format!("0 {} {} {} {} {}", fields[0], fields[1], fields[2], fields[3], dow)
        }
        6 | 7 => fields.join(" "),
        n => return Err(format!("需要 5 段（分 时 日 月 周），实际 {n} 段")),
    };
    Schedule::from_str(&normalized).map_err(|e| e.to_string())
}

/// 标准星期编号（0/7=周日, 1=周一…6=周六）→ crate 编号（1=周日…7=周六）。
fn convert_dow(field: &str) -> Result<String, String> {
    let mut out = Vec::new();
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("星期字段里有空项".to_string());
        }
        if part == "*" {
            out.push("*".to_string());
            continue;
        }
        // 形如 */2、1-5、1-5/2、3
        let (body, step) = match part.split_once('/') {
            Some((b, s)) => (b, Some(s.parse::<u32>().map_err(|_| format!("步长不合法: {s}"))?)),
            None => (part, None),
        };
        if body == "*" {
            out.push(match step {
                Some(s) => format!("*/{s}"),
                None => "*".to_string(),
            });
            continue;
        }
        let conv = |v: &str| -> Result<u32, String> {
            let n: u32 = v.trim().parse().map_err(|_| format!("星期值不合法: {v}"))?;
            match n {
                0 | 7 => Ok(1),
                1..=6 => Ok(n + 1),
                _ => Err(format!("星期值只能是 0-7，当前: {n}")),
            }
        };
        let rendered = match body.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (conv(a)?, conv(b)?);
                if a > b {
                    return Err(format!("不支持的星期范围: {part}（只支持从小到大）"));
                }
                format!("{a}-{b}")
            }
            None => conv(body)?.to_string(),
        };
        out.push(match step {
            Some(s) => format!("{rendered}/{s}"),
            None => rendered,
        });
    }
    Ok(out.join(","))
}

fn next_after(schedule: &Schedule, from: DateTime<Local>) -> DateTime<Local> {
    schedule
        .after(&from)
        .next()
        .unwrap_or(from + chrono::TimeDelta::days(3650))
}

/// 首次计划时间：
///   - 补跑窗口内有错过的触发点 → 立刻补跑一次（返回 now）
///   - 否则 → 下一个未来的触发点
fn initial_due(
    schedule: &Schedule,
    now_local: DateTime<Local>,
    window: chrono::TimeDelta,
) -> DateTime<Local> {
    let window_start = now_local - window;
    let mut last: Option<DateTime<Local>> = None;
    for t in schedule.after(&window_start) {
        if t > now_local {
            break;
        }
        last = Some(t);
    }
    match last {
        Some(_) => now_local,
        None => next_after(schedule, now_local),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn 五段表达式补秒位() {
        assert!(parse("17 4 * * *").is_ok());
        assert!(parse("0 17 4 * * *").is_ok());
        assert!(parse("17 4").is_err());
        assert!(parse("not a cron").is_err());
    }

    #[test]
    fn 标准星期编号换算正确() {
        // 2026-11-25 是周三。时间都用 Local 构造，结果不依赖运行机器的时区。
        let at = |y, m, d, h| Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
        let from = at(2026, 11, 25, 16);
        let cases = [
            ("0 4 * * 1", at(2026, 11, 30, 4)),   // 1=周一 → 11-30
            ("0 4 * * 4", at(2026, 11, 26, 4)),   // 周四 → 11-26
            ("0 4 * * 0", at(2026, 11, 29, 4)),   // 周日 → 11-29
            ("0 4 * * 7", at(2026, 11, 29, 4)),   // 7 也当周日
            ("0 4 * * 1-5", at(2026, 11, 26, 4)), // 工作日 → 周四
        ];
        for (expr, want) in cases {
            let schedule = parse(expr).unwrap();
            let next = schedule.after(&from).next().unwrap();
            assert_eq!(next, want, "{expr} 的下一次不符");
        }
    }

    #[test]
    fn 星期字段非法要报错() {
        assert!(parse("0 4 * * 9").is_err());
        assert!(parse("0 4 * * 5-1").is_err());
        assert!(parse("0 4 * * ").is_err());
    }

    #[test]
    fn 错过的任务会立刻补跑() {
        let schedule = parse("* * * * *").unwrap(); // 每分钟
        let now = Local::now();
        let due = initial_due(&schedule, now, chrono::TimeDelta::hours(24));
        assert_eq!(due, now);
    }

    #[test]
    fn 补跑窗口为0时不补跑() {
        let schedule = parse("* * * * *").unwrap();
        let now = Local::now();
        let due = initial_due(&schedule, now, chrono::TimeDelta::zero());
        assert!(due > now);
    }

    #[test]
    fn 未来的任务不会立刻跑() {
        // 选一个未来几十年不会命中的时间组合：2 月 30 日不存在 → 用 2 月 29 日
        let schedule = parse("0 3 29 2 *").unwrap();
        let now = Local::now();
        let due = initial_due(&schedule, now, chrono::TimeDelta::hours(24));
        assert!(due > now);
    }
}

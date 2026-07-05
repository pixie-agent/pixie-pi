use std::hint::black_box;
use std::time::{Duration, Instant};

use pixie_pi::agent::tool::AgentTool;
use pixie_pi::tools::find::FindTool;
use pixie_pi::tools::grep::GrepTool;
use pixie_pi::tools::read::ReadTool;
use pixie_pi::tools::truncate::{truncate_head, truncate_tail};
use tokio_util::sync::CancellationToken;

fn build_lines(lines: usize, width: usize) -> String {
    let line = "x".repeat(width);
    let mut out = String::with_capacity((width + 1) * lines);
    for _ in 0..lines {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn bench_sync<F>(name: &str, iterations: usize, mut f: F)
where
    F: FnMut(),
{
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        f();
        samples.push(started.elapsed());
    }
    report(name, &samples);
}

async fn bench_async<F, Fut>(name: &str, iterations: usize, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        f().await;
        samples.push(started.elapsed());
    }
    report(name, &samples);
}

fn report(name: &str, samples: &[Duration]) {
    let total: Duration = samples.iter().copied().sum();
    let avg = total.as_secs_f64() * 1_000.0 / samples.len() as f64;
    let min = samples.iter().min().unwrap().as_secs_f64() * 1_000.0;
    println!("{name}: avg={avg:.3}ms min={min:.3}ms n={}", samples.len());
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let big = build_lines(250_000, 80);
    println!("dataset: {:.1}MB", big.len() as f64 / 1024.0 / 1024.0);

    bench_sync("truncate_head_20mb", 20, || {
        black_box(truncate_head(black_box(&big), None, None));
    });
    bench_sync("truncate_tail_20mb", 20, || {
        black_box(truncate_tail(black_box(&big), None, None));
    });

    let path = std::env::temp_dir().join(format!(
        "pixie-pi-perf-{}.txt",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&path, &big)?;
    let tool = ReadTool {
        cwd: std::env::temp_dir(),
    };
    let args = serde_json::json!({ "path": path.to_string_lossy() });

    bench_async("read_tool_large_file", 10, || {
        let tool = &tool;
        let args = args.clone();
        async move {
            black_box(
                tool.execute(args, CancellationToken::new())
                    .await
                    .expect("read tool"),
            );
        }
    })
    .await;

    let grep = GrepTool {
        cwd: std::env::temp_dir(),
    };
    let grep_args = serde_json::json!({
        "pattern": "x",
        "path": path.to_string_lossy(),
        "limit": 10
    });
    bench_async("grep_limit_10_large_file", 10, || {
        let grep = &grep;
        let grep_args = grep_args.clone();
        async move {
            black_box(
                grep.execute(grep_args, CancellationToken::new())
                    .await
                    .expect("grep tool"),
            );
        }
    })
    .await;

    let _ = std::fs::remove_file(path);

    let dir = std::env::temp_dir().join(format!(
        "pixie-pi-find-perf-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir)?;
    for i in 0..5_000 {
        std::fs::write(dir.join(format!("file-{i:05}.txt")), "x")?;
    }
    let find = FindTool { cwd: dir.clone() };
    let find_args = serde_json::json!({
        "pattern": "*.txt",
        "path": dir.to_string_lossy(),
        "limit": 10
    });
    bench_async("find_limit_10_in_5000_files", 10, || {
        let find = &find;
        let find_args = find_args.clone();
        async move {
            black_box(
                find.execute(find_args, CancellationToken::new())
                    .await
                    .expect("find tool"),
            );
        }
    })
    .await;

    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

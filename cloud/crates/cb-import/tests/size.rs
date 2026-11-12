//! Where the bytes of an imported database go, with synthetic rows whose documents
//! have the sizes of a stock project's. Run by hand:
//! `cargo test -p cb-import --test size -- --ignored --nocapture`

use cb_import::schema::{apply_migrations, table_info};
use cb_import::value::Value;
use rusqlite::Connection;

fn row(c: &Connection, table: &str, set: &[(&str, Value)]) {
    let t = table_info(c, table).unwrap();
    let mut vals: Vec<Value> = (0..t.cols.len()).map(|i| t.default_of(i)).collect();
    for (k, v) in set {
        vals[t.index(k).unwrap()] = v.clone();
    }
    c.prepare_cached(&t.insert_sql())
        .unwrap()
        .execute(rusqlite::params_from_iter(vals.iter()))
        .unwrap();
}

fn text(n: usize, seed: usize) -> Value {
    let mut s = String::with_capacity(n);
    let mut x = seed as u64 * 2654435761 + 1;
    while s.len() < n {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.push_str(&format!("<v>{:x}</v>\n", x >> 20));
    }
    s.truncate(n);
    Value::Text(s)
}

#[test]
#[ignore]
fn where_the_bytes_go() {
    let wus: i64 = std::env::var("CB_SIZE_WORKUNITS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    let results = wus * 8 / 5; // 80,000 results for 50,000 workunits
    let c = Connection::open_in_memory().unwrap();
    apply_migrations(&c).unwrap();
    c.execute_batch("BEGIN").unwrap();
    for i in 1..=wus {
        row(
            &c,
            "workunit",
            &[
                ("id", Value::Int(i)),
                ("name", Value::Text(format!("project_batch_0001_{i:08}"))),
                ("xml_doc", text(1300, i as usize)),
                ("result_template", text(1100, 7)),
                ("rsc_fpops_est", Value::Real(1e12)),
                ("rsc_fpops_bound", Value::Real(1e13)),
                ("rsc_memory_bound", Value::Real(1e8)),
                ("rsc_disk_bound", Value::Real(1e8)),
                ("transition_time", Value::Int(2147483647)),
                ("delay_bound", Value::Int(86400)),
                ("min_quorum", Value::Int(1)),
                ("target_nresults", Value::Int(1)),
                ("max_error_results", Value::Int(3)),
                ("max_total_results", Value::Int(5)),
                ("max_success_results", Value::Int(2)),
                ("create_time", Value::Int(1_700_000_000 + i)),
                ("appid", Value::Int(1)),
            ],
        );
    }
    for i in 1..=results {
        row(
            &c,
            "result",
            &[
                ("id", Value::Int(i)),
                ("workunitid", Value::Int(i * wus / results)),
                (
                    "name",
                    Value::Text(format!(
                        "project_batch_0001_{:08}_{}",
                        i * wus / results,
                        i % 2
                    )),
                ),
                ("xml_doc_in", text(2300, i as usize)),
                ("xml_doc_out", text(900, i as usize + 1)),
                ("stderr_out", text(1800, i as usize + 2)),
                ("server_state", Value::Int(5)),
                ("outcome", Value::Int(1)),
                ("validate_state", Value::Int(1)),
                ("hostid", Value::Int(i % 3000 + 1)),
                ("userid", Value::Int(i % 900 + 1)),
                ("received_time", Value::Int(1_700_000_000 + i)),
                ("appid", Value::Int(1)),
            ],
        );
    }
    for i in 1..=(wus * 28 / 10) {
        row(
            &c,
            "job_file",
            &[
                (
                    "name",
                    Value::Text(format!("project_batch_0001_{i:08}_input_file_{}", i % 2)),
                ),
                ("create_time", Value::Int(1_700_000_000)),
                ("nbytes", Value::Int(123_456)),
                ("md5", Value::Text(format!("{:032x}", i as u128 * 7919))),
            ],
        );
    }
    c.execute_batch("COMMIT").unwrap();
    let total: i64 = c
        .query_row("SELECT sum(pgsize) FROM dbstat", [], |r| r.get(0))
        .unwrap();
    println!(
        "workunits {wus}, results {results}, job_file {}",
        wus * 28 / 10
    );
    println!("total bytes {total}");
    let mut st = c
        .prepare("SELECT name, sum(pgsize), sum(payload) FROM dbstat GROUP BY name HAVING sum(pgsize) > 100000 ORDER BY 2 DESC")
        .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .unwrap();
    for r in rows {
        let (n, p, pay) = r.unwrap();
        println!("{n:<28} pages {p:>12}  payload {pay:>12}");
    }
}

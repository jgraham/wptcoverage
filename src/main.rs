use serde::{Deserialize};
use reqwest;
use serde_json;
use std::cmp;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use structopt::StructOpt;

static BASE_URL: &str =  "https://api.coverage.testing.moz.tools/v2";

#[derive(Debug)]
pub enum Error {
    Reqwest(reqwest::Error),
    Serde(serde_json::Error),
    Io(io::Error),
    String(String)
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Error {
        Error::Reqwest(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Error {
        Error::Serde(error)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Error {
        Error::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;


pub fn get(client:&reqwest::Client, url:&str, headers: Option<reqwest::header::HeaderMap>) -> Result<String> {
    // TODO - If there's a list then support continuationToken
    eprintln!("DEBUG: GET {}", url);
    let mut req = client.get(url);
    if let Some(extra_headers) = headers {
        req = req.headers(extra_headers)
    }
    let mut resp = req.send()?;
    resp.error_for_status_ref()?;
    let mut resp_body = match resp.content_length() {
        Some(len) => String::with_capacity(len as usize),
        None => String::new()
    };
    resp.read_to_string(&mut resp_body)?;
    Ok(resp_body)
}

#[derive(Debug, Deserialize)]
struct PathCoverage {
    changeset: String,
    children: Option<Vec<FileCoverage>>,
    coveragePercent: f64,
    linesCovered: i64,
    linesMissed: i64,
    linesTotal: i64,
    name: String,
    path: String,
    #[serde(rename="type")]
    path_type: String,
    coverage: Option<Vec<i64>>
}


#[derive(Debug, Deserialize)]
struct FileCoverage {
    children: Option<i64>,
    coveragePercent: f64,
    linesCovered: i64,
    linesMissed: i64,
    linesTotal: i64,
    name: String,
    path: String,
    #[serde(rename="type")]
    path_type: String,
    coverage: Option<Vec<i64>>
}

type CoverageMap = BTreeMap<String, PathCoverage>;

fn get_suite_data(client: &reqwest::Client,
                  changeset: &str,
                  root_path: &Path,
                  suite_name: &str,
                  gecko_roots: &[&str]) -> Result<CoverageMap> {

    let mut suite_root = root_path.to_owned();
    suite_root.push(PathBuf::from(suite_name));
    let mut rv = BTreeMap::new();

    if !suite_root.exists() {
        fs::create_dir_all(&suite_root)?;
    }

    let mut stack: Vec<String> = Vec::new();
    for root in gecko_roots.iter() {
        stack.push((*root).to_owned());
    }

    while let Some(gecko_path) = stack.pop() {
        let mut local_path = suite_root.clone();
        local_path.push(PathBuf::from(format!("{}.json", gecko_path.replace("/", "-"))));

        if !local_path.exists() {
            let url = format!("{}/path?path={}&suite={}&changeset={}",
                              BASE_URL,
                              gecko_path,
                              suite_name,
                              changeset);
            let resp_str = get(&client,
                               &url,
                               None)?;
            let mut f = File::create(&local_path)?;
            f.write_all(&resp_str.as_bytes())?;
        };

        let f = File::open(&local_path)?;
        let data: PathCoverage = serde_json::from_reader(f)?;

        if let Some(ref children) = data.children {
            for file in children.iter() {
                stack.push(file.path.clone());
            }
        }

        rv.insert(gecko_path.clone(), data);
    }

    Ok(rv)
}

#[derive(Debug)]
enum CoverageType {
    NotRun,
    NotCovered,
    MochitestOnly,
    WptOnly,
    Both
}

struct CoverageDifference {
    line_differences: Vec<CoverageType>,
    line_count: i64,
    coverable_count: i64,
    covered_count: i64,
    mochitest_only_count: i64,
    wpt_only_count: i64,
    both_count: i64,
}

fn coverage_difference(wpt_coverage: &[i64], mochitest_coverage:&[i64]) -> CoverageDifference {
    let mut line_differences = Vec::new();
    let mut mochitest_only_count = 0;
    let mut wpt_only_count = 0;
    let mut both_count = 0;

    let line_count = if mochitest_coverage.len() != wpt_coverage.len() {
        eprintln!("WARNING: line counts differ");
        cmp::min(wpt_coverage.len(), mochitest_coverage.len())
    } else {
        wpt_coverage.len()
    } as i64;

    let mut coverable_count = line_count;
    for (wpt_hit_count, mochitest_hit_count) in wpt_coverage.iter().zip(mochitest_coverage.iter()) {
        let coverage_type = match (wpt_hit_count, mochitest_hit_count) {
            (-1, -1) => {
                coverable_count -= 1;
                CoverageType::NotRun
            },
            (0, 0) => {
                CoverageType::NotCovered
            },
            (x, y) if *x > 0 && *y <= 0 => {
                wpt_only_count += 1;
                CoverageType::WptOnly
            },
            (x, y) if *x <= 0 && *y > 0 => {
                mochitest_only_count += 1;
                CoverageType::MochitestOnly
            },
            (_, _) => {
                both_count += 1;
                CoverageType::Both
            }
        };
        //println!("{} {} {:?}", wpt_hit_count, mochitest_hit_count, coverage_type);
        line_differences.push(coverage_type);
    }
    let mut covered_count = both_count + wpt_only_count + mochitest_only_count;
    CoverageDifference {
        line_differences,
        line_count,
        coverable_count,
        covered_count,
        mochitest_only_count,
        wpt_only_count,
        both_count,
    }
}


fn get_differences(wpt_data: CoverageMap, mochitest_data: CoverageMap) -> BTreeMap<String, CoverageDifference> {
    let mut rv = BTreeMap::new();
    for (path, wpt_coverage) in wpt_data.iter() {
        if wpt_coverage.path_type == "directory" {
            continue;
        }
        if let Some(ref wpt_coverage_vec) = wpt_coverage.coverage {
            if let Some(ref mochitest_coverage) = mochitest_data.get(path) {
                if let Some(ref mochitest_coverage_vec) = mochitest_coverage.coverage {
                    rv.insert(path.clone(),
                              coverage_difference(wpt_coverage_vec, mochitest_coverage_vec));
                }
            }
        }
    }
    rv
}

fn get_latest_changeset(client: &reqwest::Client) -> Result<String> {
    let resp_str = get(&client,
                       &format!("{}/path?path=", BASE_URL),
                       None)?;
    let data: PathCoverage = serde_json::from_str(&resp_str)?;
    Ok(data.changeset)
}

#[derive(Debug, StructOpt)]
#[structopt(name = "wptcoverage", about = "Download and process wpt coverage data")]
struct Opt {
    changeset: Option<String>
}


fn run() -> Result<()> {
    let client = reqwest::Client::new();

    let opt = Opt::from_args();
    let changeset = opt.changeset
        .map(|x| Ok(x))
        .unwrap_or_else(|| get_latest_changeset(&client))?;

    let base_path = PathBuf::from(format!("data/{}", changeset));

    let wpt_data = get_suite_data(&client, &changeset, &base_path, "web-platform-tests", &["dom"])?;
    let mochitest_data = get_suite_data(&client, &changeset, &base_path, "mochitest-plain-chunked", &["dom"])?;

    let differences = get_differences(wpt_data, mochitest_data);

    println!("path, wpt only, mochitest only, both, total covered, total coverable, total lines, wpt-only percent, mochitest-only percent, coverage percent");
    for (path, coverage_difference) in differences.iter() {

        let percent = |count: i64| {
            100f64 * count as f64 / coverage_difference.coverable_count as f64
        };

        println!("\"{}\", {}, {}, {}, {}, {}, {}, {}, {}, {}",
                 path,
                 coverage_difference.wpt_only_count,
                 coverage_difference.mochitest_only_count,
                 coverage_difference.both_count,
                 coverage_difference.covered_count,
                 coverage_difference.coverable_count,
                 coverage_difference.line_count,
                 percent(coverage_difference.wpt_only_count),
                 percent(coverage_difference.mochitest_only_count),
                 percent(coverage_difference.covered_count),
        );
    }

    Ok(())
}

fn main() {
    if let Err(e) =  run() {
        eprintln!("ERROR: Failed:\n{:?}", e);
        process::exit(1);
    };

}

mod collect;
mod jobs;

use std::{collections::HashMap, fmt::Write as FmtWrite};
use std::io::Write; // Import the Write trait

use clap::Parser;
use indicatif::{ProgressState, ProgressStyle};
use jobs::Jobs;
use measure::{MeasureRequest, MeasureResponse};
use reqwest::ClientBuilder;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
pub struct CliArgs {
    /// The url the measure service will be calling the http `get` method` on
    target_request_url: Option<String>,

    /// The comparison url the measure service will be calling the http `get` method` on
    #[clap(long = "comp")]
    comparison_url: Option<String>,

    /// The ip address of the measure services
    #[clap(long)]
    services: Option<Vec<String>>,

    /// Compute and print the average of the results
    #[clap(short, long)]
    average: bool,

    /// The number of times to get a latency measurement from service
    #[clap(short, long, default_value_t = 10)]
    times: usize,

    /// The delay in milliseconds between each measurement
    #[clap(short, long, default_value_t = 500)]
    delay: usize,

    /// The output file to write the json results to
    #[clap(short, long)]
    output_dir: Option<String>,

    /// Creates requests concurrently rather than sequentially
    /// and ignores the delay param
    #[clap(long)]
    flood: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = CliArgs::parse();

    let _ = Runtime::new(args)?.start().await?;

    Ok(())
}

#[derive(Debug)]
struct Runtime {
    jobs: Jobs,
    results: HashMap<String, Vec<MeasureResponse>>,
    comparison_results: Option<HashMap<String, Vec<MeasureResponse>>>,
    output_dir: Option<String>,
    average: bool,
    times: usize,
    delay: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct Output {
    /// mapping from service ip to the results of the target url
    target_results: HashMap<String, Vec<MeasureResponse>>,
    /// mapping from service ip to the results of the comparison url
    comparison_results: Option<HashMap<String, Vec<MeasureResponse>>>,
}

impl Runtime {
    fn new(args: CliArgs) -> anyhow::Result<Self> {
        Ok(Runtime {
            jobs: args.jobs()?,
            results: HashMap::new(),
            comparison_results: args.comparison_url.map(|_| HashMap::new()),
            average: args.average,
            times: args.times,
            delay: args.delay,
            output_dir: args.output_dir,
        })
    }

    async fn start(mut self) -> anyhow::Result<()> {
        let Jobs {
            services,
            target_url,
            comparison_url,
        } = self.jobs.clone();

        let mut csv_output = String::new();

        // Add CSV headers
        writeln!(csv_output, "Service IP,Type,{}", (1..=self.times).map(|i| format!("Attempt {}", i)).collect::<Vec<_>>().join(",")).unwrap();

        for service_ip in services {
            println!("running for: {}", service_ip);
            self.run(service_ip.clone(), target_url.clone(), comparison_url.clone())
                .await?;

            // Add results for the target URL
            if let Some(results) = self.results.get(&service_ip) {
                writeln!(
                    csv_output,
                    "{},{},{}",
                    service_ip,
                    target_url,
                    results
                        .iter()
                        .map(|res| res.ttfb_duration.as_millis().to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                ).unwrap();
            }

            // Add results for the comparison URL, if any
            if let Some(comp_results) = &self.comparison_results {
                if let Some(results) = comp_results.get(&service_ip) {
                    writeln!(
                        csv_output,
                        "{},{},{}",
                        service_ip,
                        comparison_url.as_ref().unwrap(),
                        results
                            .iter()
                            .map(|res| res.ttfb_duration.as_millis().to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    ).unwrap();
                }
            }
        }

        println!("{}", csv_output);

        if let Some(ref dir) = self.output_dir {
            // There's no other tasks running so blocking is acceptable
            std::fs::create_dir_all(dir)?;

            let timestamp = chrono::Utc::now().to_rfc3339();
            let mut file = std::fs::File::create(format!("{}/{}.csv", dir, timestamp))?;

            file.write_all(csv_output.as_bytes())?; // Write the CSV data to the file
        }

        Ok(())
    }

    async fn run(
        &mut self,
        service_ip: String,
        target_url: String,
        maybe_comp: Option<String>,
    ) -> anyhow::Result<()> {
        let req = ClientBuilder::new()
            .build()?
            .post(&service_ip)
            .json(&MeasureRequest {
                target: target_url.clone(),
            });

        println!("measuring target ttfb");
        self.results.insert(
            service_ip.clone(),
            Self::measure(req, self.times, self.delay).await?,
        );

        if let Some(ref url) = maybe_comp {
            let comparison_req =
                ClientBuilder::new()
                    .build()?
                    .post(&service_ip)
                    .json(&MeasureRequest {
                        target: url.clone(),
                    });

            println!("measuring comparison ttfb");
            self.comparison_results
                .as_mut()
                .expect("comparison results")
                .insert(
                    service_ip.clone(),
                    Self::measure(comparison_req, self.times, self.delay).await?,
                );
        }

        if self.average {
            let target = collect::average(
                self.results
                    .get(&service_ip)
                    .expect("results for this ip")
                    .iter(),
                self.times,
            );

            print_average(target_url, target);

            match self.comparison_results {
                Some(ref comp) => {
                    let comp = collect::average(
                        comp.get(&service_ip).expect("results for this ip").iter(),
                        self.times,
                    );

                    print_average(maybe_comp.expect("comparison url"), comp);
                }
                None => (),
            };
        }

        Ok(())
    }

    async fn measure(
        req: reqwest::RequestBuilder,
        times: usize,
        delay: usize,
    ) -> anyhow::Result<Vec<MeasureResponse>> {
        let mut buf = Vec::with_capacity(times);
        let pb = indicatif::ProgressBar::new(times as u64);

        pb.set_style(ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .with_key("eta", |state: &ProgressState, w: &mut dyn FmtWrite| write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap())
            .progress_chars("#>-"));

        for i in 0..times {
            let cloned = req
                .try_clone()
                .ok_or(anyhow::anyhow!("failed to clone request"))?;

            let res = cloned.send().await?.json::<MeasureResponse>().await?;

            buf.push(res);

            pb.set_position(i as u64);

            tokio::time::sleep(tokio::time::Duration::from_millis(delay as u64)).await;
        }

        Ok(buf)
    }

    fn output(&self) -> Output {
        Output {
            target_results: self.results.clone(),
            comparison_results: self.comparison_results.clone(),
        }
    }
}

fn print_average(label: String, measure: MeasureResponse) {
    println!("URL: {:#?}", label);
    println!("Average: {}ms", measure.ttfb_duration.as_millis());
}

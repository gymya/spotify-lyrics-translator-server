use shared::{TranslationRequest, queue::Queue};
use std::{io::Read, time::Instant};
use uuid::Uuid;

fn read_request(path: &str) -> Result<TranslationRequest, Box<dyn std::error::Error>> {
    let reader: Box<dyn Read> = if path == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(path)?)
    };
    let mut input = Vec::new();
    reader.take(65537).read_to_end(&mut input)?;
    if input.len() > 65536 {
        return Err("request JSON exceeds 64 KiB".into());
    }
    let request: TranslationRequest =
        serde_json::from_slice(&input).map_err(|_| "invalid request JSON")?;
    request.validate()?;
    Ok(request)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    shared::init_logging();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if let [command @ ("translate" | "benchmark"), path] = args.as_slice() {
        let request = read_request(path)?;
        let ollama = worker::Ollama::from_env()?;
        let start = Instant::now();
        let result = tokio::select! {
            result = ollama.translate(&request) => result?,
            _ = shared::shutdown_signal() => return Err("translation interrupted".into()),
        };
        let output = if *command == "benchmark" {
            serde_json::json!({"model":ollama.model,"elapsedMs":start.elapsed().as_millis(),"sourceLanguage":request.source_language,"lineCount":request.lyrics.len(),"translations":result.translations})
        } else {
            serde_json::to_value(result)?
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    if !matches!(
        args.as_slice(),
        ["migrate" | "run" | "cleanup"] | ["enqueue" | "status", _, _]
    ) {
        return Err("usage: worker translate|benchmark <file|->; migrate; run; cleanup; enqueue <user-uuid> <file|->; status <user-uuid> <job-uuid>".into());
    }
    let url = std::env::var("DATABASE_URL").map_err(|_| "set DATABASE_URL")?;
    let queue = Queue::connect(&url).await?;
    match args.as_slice() {
        ["migrate"] => queue.migrate().await?,
        ["cleanup"] => println!("{}", serde_json::json!({"deleted":queue.cleanup().await?})),
        ["run"] => worker::runner::run(queue, worker::Ollama::from_env()?).await,
        ["enqueue", user, path] => {
            let model = std::env::var("OLLAMA_MODEL").map_err(|_| "set OLLAMA_MODEL")?;
            let job = queue
                .enqueue(Uuid::parse_str(user)?, &read_request(path)?, &model)
                .await?;
            println!("{}", serde_json::to_string_pretty(&job)?);
        }
        ["status", user, id] => {
            let job = queue
                .get(Uuid::parse_str(user)?, Uuid::parse_str(id)?)
                .await?
                .ok_or("job not found")?;
            println!("{}", serde_json::to_string_pretty(&job)?);
        }
        _ => unreachable!(),
    }
    Ok(())
}

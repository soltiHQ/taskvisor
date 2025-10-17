use taskvisor::{Config, ControllerConfig, Supervisor, controller};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::default();
    let subs = Vec::new();
    let sup = Supervisor::new(cfg, subs);

    sup.run(vec![controller(ControllerConfig::new())]).await?;
    Ok(())
}

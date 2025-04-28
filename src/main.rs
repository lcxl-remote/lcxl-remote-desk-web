use log::{error, info};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Start the Actix web server here
    let run_result = lcxl_web_remote_desk::run().await;
    match run_result {
        Ok(server) => {
            info!("Server started successfully");
            return server.await;
        }
        Err(e) => {
            error!("Failed to start server: {:?}", e);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("{:?}", e),
            ));
        }
    }
}

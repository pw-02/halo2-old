use ec_gpu_gen::EcError;

#[derive(thiserror::Error, Debug)]
pub enum GpuError {
    #[error("GPUError: {0}")]
    Simple(&'static str),
    #[cfg(any(feature = "cuda", feature = "opencl"))]
    #[error("No kernel is initialized!")]
    KernelUninitialized,
    #[error("EC GPU error: {0}")]
    EcGpu(#[from] EcError),
    #[error("GPU accelerator is disabled!")]
    GpuDisabled,
}

pub type GpuResult<T> = std::result::Result<T, GpuError>;

impl From<std::boxed::Box<dyn std::any::Any + std::marker::Send>> for GpuError {
    fn from(e: std::boxed::Box<dyn std::any::Any + std::marker::Send>) -> Self {
        match e.downcast::<Self>() {
            Ok(err) => *err,
            Err(_) => GpuError::Simple("An unknown GPU error happened!"),
        }
    }
}

//impl From<GpuError> for SynthesisError {
//    fn from(e: GpuError) -> Self {
//        // inspired by the commenct on MalformedProofs
//        SynthesisError::MalformedProofs(format!("Encountered a GPU Error: {}", e))
//    }
//}
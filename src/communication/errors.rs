use std::io;
use tokio::sync::mpsc;

/// Error raised by the communication layer.
#[derive(Debug)]
pub enum CommunicationError {
    /// The channel has no capacity left.
    NoCapacity,
    /// The channel or the TCP stream has been closed.
    Disconnected,
    /// Type does not support serialization.
    SerializeNotImplemented,
    /// Type does not support deserialization.
    DeserializeNotImplemented,
    /// Failed to serialize/deserialize data with Abomonation.
    AbomonationError(io::Error),
    /// Failed to serialize/deserialize data with Bincode.
    BincodeError(bincode::Error),
    /// Failed to read/write data from/to the TCP stream.
    IoError(io::Error),
    /// Error from Zenoh layer
    #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
    ZenohError(zenoh::ZError),
    /// Errors from Shared Memory
    #[cfg(feature = "zenoh_zerocopy_transport")]
    SharedMemoryError(shared_memory::ShmemError),
    #[cfg(feature = "zenoh_zerocopy_transport")]
    ZenohSharedMemoryError(String),
}

impl From<bincode::Error> for CommunicationError {
    fn from(e: bincode::Error) -> Self {
        CommunicationError::BincodeError(e)
    }
}

impl From<io::Error> for CommunicationError {
    fn from(e: io::Error) -> Self {
        CommunicationError::IoError(e)
    }
}

impl<T> From<std::sync::mpsc::SendError<T>> for CommunicationError {
    fn from(_e: std::sync::mpsc::SendError<T>) -> Self {
        CommunicationError::Disconnected
    }
}

impl<T> From<mpsc::error::SendError<T>> for CommunicationError {
    fn from(_e: mpsc::error::SendError<T>) -> Self {
        CommunicationError::Disconnected
    }
}

impl<T> From<mpsc::error::TrySendError<T>> for CommunicationError {
    fn from(e: mpsc::error::TrySendError<T>) -> Self {
        match e {
            mpsc::error::TrySendError::Closed(_) => CommunicationError::Disconnected,
            mpsc::error::TrySendError::Full(_) => CommunicationError::NoCapacity,
        }
    }
}

impl From<CodecError> for CommunicationError {
    fn from(e: CodecError) -> Self {
        match e {
            CodecError::IoError(e) => CommunicationError::IoError(e),
            CodecError::BincodeError(e) => CommunicationError::BincodeError(e),
            #[cfg(feature = "zenoh_zerocopy_transport")]
            CodecError::SharedMemoryError(shm_error) => {
                CommunicationError::SharedMemoryError(shm_error)
            }
            #[cfg(feature = "zenoh_zerocopy_transport")]
            CodecError::ZenohSharedMemoryError(zshm_error) => {
                CommunicationError::ZenohSharedMemoryError(zshm_error)
            }
        }
    }
}

#[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
impl From<zenoh::ZError> for CommunicationError {
    fn from(e: zenoh::ZError) -> Self {
        CommunicationError::ZenohError(e)
    }
}

#[cfg(feature = "zenoh_zerocopy_transport")]
impl From<shared_memory::ShmemError> for CommunicationError {
    fn from(e: shared_memory::ShmemError) -> Self {
        CommunicationError::SharedMemoryError(e)
    }
}

/// Error that is raised by the `MessageCodec` when messages cannot be encoded or decoded.
#[derive(Debug)]
pub enum CodecError {
    IoError(io::Error),
    /// Bincode serialization/deserialization error. It is raised when the `MessageMetadata` serialization
    /// fails. This should not ever happen.
    BincodeError(bincode::Error),
    /// Error from Shared Memory
    #[cfg(feature = "zenoh_zerocopy_transport")]
    SharedMemoryError(shared_memory::ShmemError),
    #[cfg(feature = "zenoh_zerocopy_transport")]
    ZenohSharedMemoryError(String),
}

impl From<io::Error> for CodecError {
    fn from(e: io::Error) -> CodecError {
        CodecError::IoError(e)
    }
}

impl From<bincode::Error> for CodecError {
    fn from(e: bincode::Error) -> Self {
        CodecError::BincodeError(e)
    }
}

#[cfg(feature = "zenoh_zerocopy_transport")]
impl From<shared_memory::ShmemError> for CodecError {
    fn from(e: shared_memory::ShmemError) -> Self {
        CodecError::SharedMemoryError(e)
    }
}

#[derive(Debug)]
pub enum TryRecvError {
    /// No data to read.
    Empty,
    /// The channel or the TCP stream has been closed.
    Disconnected,
    /// Failed to serialize/deserialize data.
    BincodeError(bincode::Error),
}

impl From<mpsc::error::TryRecvError> for TryRecvError {
    fn from(e: mpsc::error::TryRecvError) -> Self {
        match e {
            mpsc::error::TryRecvError::Closed => TryRecvError::Disconnected,
            mpsc::error::TryRecvError::Empty => TryRecvError::Empty,
        }
    }
}

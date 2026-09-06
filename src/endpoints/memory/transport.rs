//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::CanonicalMessage;
use anyhow::{anyhow, Result};
use async_trait::async_trait;

/// Transport URL scheme and path information
#[derive(Debug, Clone, PartialEq)]
pub enum TransportUrl {
    /// In-process memory transport: memory://namespace
    Memory { namespace: String },
    /// Unix Domain Socket: unix:///path or ipc://name or ipc:///path
    #[cfg(unix)]
    Unix { path: String },
    /// Windows Named Pipe: pipe://name or ipc://name
    #[cfg(windows)]
    Pipe { name: String },
}

impl TransportUrl {
    /// Parse a transport URL string
    pub fn parse(url: &str) -> Result<Self> {
        if url.is_empty() {
            return Err(anyhow!("Transport URL cannot be empty"));
        }

        // Check for scheme
        if let Some((scheme, rest)) = url.split_once("://") {
            match scheme {
                "memory" if rest.is_empty() => Err(anyhow!("Memory namespace cannot be empty")),
                "memory" => Ok(TransportUrl::Memory {
                    namespace: rest.to_string(),
                }),
                #[cfg(unix)]
                "unix" => {
                    if !rest.starts_with('/') {
                        return Err(anyhow!("Unix socket path must be absolute: {}", url));
                    }
                    Ok(TransportUrl::Unix {
                        path: rest.to_string(),
                    })
                }
                #[cfg(windows)]
                "pipe" if rest.is_empty() => Err(anyhow!("Pipe name cannot be empty")),
                #[cfg(windows)]
                "pipe" => Ok(TransportUrl::Pipe {
                    name: rest.to_string(),
                }),
                "ipc" => {
                    if rest.is_empty() {
                        return Err(anyhow!("IPC transport identifier cannot be empty"));
                    }
                    // Platform-specific IPC handling
                    #[cfg(unix)]
                    {
                        if rest.starts_with('/') {
                            // Explicit path: ipc:///absolute/path
                            Ok(TransportUrl::Unix {
                                path: rest.to_string(),
                            })
                        } else {
                            // Named socket: ipc://name -> /run/mq-bridge/name.sock
                            let path = Self::default_unix_socket_path(rest)?;
                            Ok(TransportUrl::Unix { path })
                        }
                    }
                    #[cfg(windows)]
                    {
                        // Named pipe: ipc://name -> \\.\pipe\mq-bridge-name
                        Ok(TransportUrl::Pipe {
                            name: format!("mq-bridge-{}", rest),
                        })
                    }
                    #[cfg(not(any(unix, windows)))]
                    {
                        Err(anyhow!("IPC transport not supported on this platform"))
                    }
                }
                _ => Err(anyhow!("Unknown transport scheme: {}", scheme)),
            }
        } else {
            // No scheme, treat as memory namespace for backward compatibility
            Ok(TransportUrl::Memory {
                namespace: url.to_string(),
            })
        }
    }

    #[cfg(unix)]
    fn default_unix_socket_path(name: &str) -> Result<String> {
        // Try /run/mq-bridge first (systemd standard)
        let run_dir = std::path::Path::new("/run/mq-bridge");
        if run_dir.exists() || std::fs::create_dir_all(run_dir).is_ok() {
            return Ok(format!("/run/mq-bridge/{}.sock", name));
        }

        // Try XDG_RUNTIME_DIR
        if let Ok(xdg_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let mq_dir = std::path::Path::new(&xdg_dir).join("mq-bridge");
            if mq_dir.exists() || std::fs::create_dir_all(&mq_dir).is_ok() {
                return Ok(format!("{}/{}.sock", mq_dir.display(), name));
            }
        }

        // Fallback to /tmp (less secure)
        let tmp_dir = std::path::Path::new("/tmp/mq-bridge");
        std::fs::create_dir_all(tmp_dir)?;
        Ok(format!("/tmp/mq-bridge/{}.sock", name))
    }

    /// Get a display name for logging
    pub fn display_name(&self) -> String {
        match self {
            TransportUrl::Memory { namespace } => format!("memory://{}", namespace),
            #[cfg(unix)]
            TransportUrl::Unix { path } => format!("unix://{}", path),
            #[cfg(windows)]
            TransportUrl::Pipe { name } => format!("pipe://{}", name),
        }
    }

    /// Get the namespace/identifier for channel lookup
    pub fn namespace(&self) -> String {
        match self {
            TransportUrl::Memory { namespace } => namespace.clone(),
            #[cfg(unix)]
            TransportUrl::Unix { path } => path.clone(),
            #[cfg(windows)]
            TransportUrl::Pipe { name } => name.clone(),
        }
    }
}

/// Abstract transport channel for sending/receiving message batches
#[async_trait]
pub trait TransportChannel: Send + Sync {
    /// Send a batch of messages
    async fn send_batch(&self, messages: Vec<CanonicalMessage>) -> Result<()>;

    /// Receive a batch of messages (blocking)
    async fn recv_batch(&self) -> Result<Vec<CanonicalMessage>>;

    /// Try to receive a batch without blocking
    fn try_recv_batch(&self) -> Result<Option<Vec<CanonicalMessage>>>;

    /// Get the current number of batches in the channel
    fn len(&self) -> usize;

    /// Check if the channel is empty
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the capacity of the channel
    fn capacity(&self) -> Option<usize>;

    /// Check if the channel is closed
    fn is_closed(&self) -> bool;

    /// Close the channel
    fn close(&self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_memory_url() {
        let url = TransportUrl::parse("memory://test-topic").unwrap();
        assert_eq!(
            url,
            TransportUrl::Memory {
                namespace: "test-topic".to_string()
            }
        );
    }

    #[test]
    fn test_parse_legacy_topic() {
        let url = TransportUrl::parse("test-topic").unwrap();
        assert_eq!(
            url,
            TransportUrl::Memory {
                namespace: "test-topic".to_string()
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_unix_socket() {
        let url = TransportUrl::parse("unix:///tmp/test.sock").unwrap();
        assert_eq!(
            url,
            TransportUrl::Unix {
                path: "/tmp/test.sock".to_string()
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_ipc_named() {
        let url = TransportUrl::parse("ipc://worker").unwrap();
        match url {
            TransportUrl::Unix { path } => {
                assert!(path.contains("worker.sock"));
            }
            _ => panic!("Expected Unix transport"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_ipc_absolute() {
        let url = TransportUrl::parse("ipc:///var/run/test.sock").unwrap();
        assert_eq!(
            url,
            TransportUrl::Unix {
                path: "/var/run/test.sock".to_string()
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_pipe() {
        let url = TransportUrl::parse("pipe://worker").unwrap();
        assert_eq!(
            url,
            TransportUrl::Pipe {
                name: "worker".to_string()
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_ipc_windows() {
        let url = TransportUrl::parse("ipc://worker").unwrap();
        assert_eq!(
            url,
            TransportUrl::Pipe {
                name: "mq-bridge-worker".to_string()
            }
        );
    }

    #[test]
    fn test_parse_empty_url() {
        let result = TransportUrl::parse("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_unknown_scheme() {
        let result = TransportUrl::parse("http://test");
        assert!(result.is_err());
    }
}

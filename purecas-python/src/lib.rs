#![allow(clippy::useless_conversion)]

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct InnerStore(purecas_core::Store);
// rusqlite::Connection is Send (not Sync), so Mutex<InnerStore> is Send + Sync automatically.

fn map_anyhow(e: anyhow::Error) -> PyErr {
    PyRuntimeError::new_err(format!("{:#}", e))
}

#[pyclass]
#[derive(Clone)]
struct Store {
    inner: Arc<Mutex<InnerStore>>,
}

#[pyclass]
#[derive(Clone)]
struct Blob {
    store: Arc<Mutex<InnerStore>>,
    hash: String,
}

#[pyclass]
#[derive(Clone)]
struct Package {
    store: Arc<Mutex<InnerStore>>,
    name: String,
}

#[pymethods]
impl Store {
    #[staticmethod]
    fn open(root: &str) -> PyResult<Self> {
        let inner = purecas_core::Store::open(PathBuf::from(root)).map_err(map_anyhow)?;
        Ok(Store {
            inner: Arc::new(Mutex::new(InnerStore(inner))),
        })
    }

    fn blob(&self, hash: &str) -> Blob {
        Blob {
            store: self.inner.clone(),
            hash: hash.to_string(),
        }
    }

    fn add_path(&self, path: &str) -> PyResult<Blob> {
        let guard = self.inner.lock().unwrap();
        let blob = guard.0.add_path(&PathBuf::from(path)).map_err(map_anyhow)?;
        let hash = blob.hash().to_string();
        drop(guard);
        Ok(Blob {
            store: self.inner.clone(),
            hash,
        })
    }

    fn add_verified_path(&self, path: &str, expected_hash: &str) -> PyResult<Blob> {
        let guard = self.inner.lock().unwrap();
        let blob = guard
            .0
            .add_verified_path(&PathBuf::from(path), expected_hash)
            .map_err(map_anyhow)?;
        let hash = blob.hash().to_string();
        drop(guard);
        Ok(Blob {
            store: self.inner.clone(),
            hash,
        })
    }

    fn add_url(&self, url: &str) -> PyResult<Blob> {
        let guard = self.inner.lock().unwrap();
        let blob = guard.0.add_url(url).map_err(map_anyhow)?;
        let hash = blob.hash().to_string();
        drop(guard);
        Ok(Blob {
            store: self.inner.clone(),
            hash,
        })
    }

    fn add_verified_url(&self, url: &str, expected_hash: &str) -> PyResult<Blob> {
        let guard = self.inner.lock().unwrap();
        let blob = guard
            .0
            .add_verified_url(url, expected_hash)
            .map_err(map_anyhow)?;
        let hash = blob.hash().to_string();
        drop(guard);
        Ok(Blob {
            store: self.inner.clone(),
            hash,
        })
    }

    fn add_url_unzip(&self, url: &str) -> PyResult<Vec<Blob>> {
        let guard = self.inner.lock().unwrap();
        let blobs = guard.0.add_url_unzip(url).map_err(map_anyhow)?;
        let result: Vec<Blob> = blobs
            .iter()
            .map(|b: &purecas_core::Blob<'_>| Blob {
                store: self.inner.clone(),
                hash: b.hash().to_string(),
            })
            .collect();
        drop(guard);
        Ok(result)
    }

    fn add_verified_url_unzip(&self, url: &str, expected_hash: &str) -> PyResult<Vec<Blob>> {
        let guard = self.inner.lock().unwrap();
        let blobs = guard
            .0
            .add_verified_url_unzip(url, expected_hash)
            .map_err(map_anyhow)?;
        let result: Vec<Blob> = blobs
            .iter()
            .map(|b: &purecas_core::Blob<'_>| Blob {
                store: self.inner.clone(),
                hash: b.hash().to_string(),
            })
            .collect();
        drop(guard);
        Ok(result)
    }

    #[pyo3(signature = (name, description=None))]
    fn create_package(&self, name: &str, description: Option<&str>) -> PyResult<Package> {
        let guard = self.inner.lock().unwrap();
        guard
            .0
            .create_package(name, description)
            .map_err(map_anyhow)?;
        drop(guard);
        Ok(Package {
            store: self.inner.clone(),
            name: name.to_string(),
        })
    }

    fn package(&self, name: &str) -> Package {
        Package {
            store: self.inner.clone(),
            name: name.to_string(),
        }
    }

    fn list_packages(&self) -> PyResult<Vec<Package>> {
        let guard = self.inner.lock().unwrap();
        let pkgs = guard.0.list_packages().map_err(map_anyhow)?;
        let result: Vec<Package> = pkgs
            .iter()
            .map(|p: &purecas_core::Package<'_>| Package {
                store: self.inner.clone(),
                name: p.name().to_string(),
            })
            .collect();
        drop(guard);
        Ok(result)
    }

    fn import_from(&self, from: &str) -> PyResult<u64> {
        let guard = self.inner.lock().unwrap();
        let result = guard.0.import(&PathBuf::from(from)).map_err(map_anyhow)?;
        Ok(result.imported_blobs)
    }
}

#[pymethods]
impl Blob {
    #[getter]
    fn hash(&self) -> &str {
        &self.hash
    }

    #[getter]
    fn path(&self) -> String {
        let guard = self.store.lock().unwrap();
        let blob = guard.0.blob(&self.hash);
        blob.path().to_string_lossy().to_string()
    }

    fn names(&self) -> PyResult<Vec<String>> {
        let guard = self.store.lock().unwrap();
        guard.0.blob(&self.hash).names().map_err(map_anyhow)
    }

    fn add_tags(&self, tags: Vec<String>) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        let tag_refs: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
        guard
            .0
            .blob(&self.hash)
            .add_tags(&tag_refs)
            .map_err(map_anyhow)
    }

    fn tags(&self) -> PyResult<Vec<String>> {
        let guard = self.store.lock().unwrap();
        guard.0.blob(&self.hash).tags().map_err(map_anyhow)
    }

    fn set_metadata(&self, value: &str) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        guard
            .0
            .blob(&self.hash)
            .set_metadata(value)
            .map_err(map_anyhow)
    }

    fn metadata(&self) -> PyResult<Option<String>> {
        let guard = self.store.lock().unwrap();
        guard.0.blob(&self.hash).metadata().map_err(map_anyhow)
    }

    #[pyo3(signature = (target, note=None))]
    fn add_relation(&self, target: &Blob, note: Option<&str>) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        let src = guard.0.blob(&self.hash);
        let tgt = guard.0.blob(&target.hash);
        src.add_relation(&tgt, note).map_err(map_anyhow)
    }

    fn relations(&self) -> PyResult<Vec<(String, Option<String>)>> {
        let guard = self.store.lock().unwrap();
        let rels = guard.0.blob(&self.hash).relations().map_err(map_anyhow)?;
        Ok(rels.into_iter().map(|r| (r.target, r.note)).collect())
    }

    fn __repr__(&self) -> String {
        format!("Blob({})", &self.hash[..8.min(self.hash.len())])
    }
}

#[pymethods]
impl Package {
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> PyResult<Option<String>> {
        let guard = self.store.lock().unwrap();
        guard
            .0
            .package(&self.name)
            .description()
            .map_err(map_anyhow)
    }

    #[pyo3(signature = (blob, path=None))]
    fn add_blob(&self, blob: &Blob, path: Option<&str>) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        let pkg = guard.0.package(&self.name);
        let b = guard.0.blob(&blob.hash);
        pkg.add_blob(&b, path).map_err(map_anyhow)
    }

    fn blobs(&self) -> PyResult<Vec<PyObject>> {
        let guard = self.store.lock().unwrap();
        let blob_infos = guard.0.package(&self.name).blobs().map_err(map_anyhow)?;
        drop(guard);
        Python::with_gil(|py| {
            blob_infos
                .iter()
                .map(|info| {
                    let dict = pyo3::types::PyDict::new_bound(py);
                    dict.set_item("hash", &info.hash)?;
                    dict.set_item("path", &info.path)?;
                    dict.set_item("names", &info.names)?;
                    Ok(dict.into())
                })
                .collect()
        })
    }

    fn remove(&self) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        guard.0.package(&self.name).remove().map_err(map_anyhow)
    }

    fn export(&self, to: &str) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        std::fs::create_dir_all(to).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        guard
            .0
            .package(&self.name)
            .export(&PathBuf::from(to))
            .map_err(map_anyhow)
    }

    fn __repr__(&self) -> String {
        format!("Package({})", self.name)
    }
}

#[pymodule]
fn purecas(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Store>()?;
    m.add_class::<Blob>()?;
    m.add_class::<Package>()?;
    Ok(())
}

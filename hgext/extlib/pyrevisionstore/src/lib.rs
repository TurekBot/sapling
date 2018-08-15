// Copyright Facebook, Inc. 2018
//! pyrevisionstore - Python interop layer for a Mercurial data and history store

#[macro_use]
extern crate cpython;
extern crate failure;
#[macro_use]
extern crate failure_derive;
extern crate pathencoding;
extern crate revisionstore;

mod datastorepyext;
mod historystorepyext;
mod pyerror;
mod pythondatastore;
mod pythonutil;
mod repackablepyext;

#[allow(non_camel_case_types)]
pub mod pyext;

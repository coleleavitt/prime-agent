//! # pa-dream
//!
//! Dream-RSI (Zheng et al., 2026): grow a discovery tree over a scored task,
//! freeze each tree into a zero-cost replay simulator, and improve a typed,
//! serializable exploration policy by searching over it on replay, with an
//! online probation for every adoption. This crate is the standalone,
//! zero-token runner behind `prime-agent dream` and the in-session feature
//! ([`session::DreamFeature`]): the LLM proposer, dreamer and guidance
//! writer ([`llm`]), the agent loop ([`llm_loop`]), the LLM experiment arms
//! ([`experiment_llm`]) and the run service ([`run_service`]).
//! See `README.md` for scope, files owned and telemetry.

pub mod agent_runner;
pub mod child;
pub mod collate;
pub mod command;
pub mod dream_loop;
pub mod dreams;
pub mod experiment;
pub mod experiment_llm;
pub mod improve;
pub mod interpreter;
pub mod js_math;
pub mod json;
pub mod llm;
pub mod llm_loop;
pub mod objective;
pub mod policy;
pub mod proposer;
pub mod records;
pub mod rejections;
pub mod replay;
pub mod requests;
pub mod rng;
pub mod rollout;
pub mod run_service;
pub mod session;
pub mod store;
pub mod task;
pub mod tasks;
pub mod tree;

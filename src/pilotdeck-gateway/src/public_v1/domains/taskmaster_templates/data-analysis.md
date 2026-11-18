# Product Requirements Document - Data Analysis Project

## Overview
**Project Name:** [Your Analysis Project]
**Analysis Type:** [Descriptive / Predictive / Prescriptive]
**Date:** [Date]
**Author:** [Your Name]

## Executive Summary
The business question, the data that will answer it, and the decision the
analysis is meant to inform.

## Project Goals
- Goal 1: [Question this analysis will answer]
- Goal 2: [Prediction or estimate this analysis will produce]
- Goal 3: [Recommendation this analysis will support]

## Business Requirements

### Key Questions
1. What patterns exist in [data domain]?
2. What factors influence [target variable]?
3. What can be projected about [future outcome]?
4. Which levers move [business metric]?

### Success Criteria
- Findings are actionable for [audience]
- Results are reproducible end to end
- Uncertainty is stated, not hidden

## Data Requirements

### Sources
1. Primary source: [system or files], format [CSV / JSON / SQL], volume
   [estimate], refresh cadence [real-time / daily / monthly]
2. Supplementary sources: [external data and licensing constraints]

### Quality Gates
- Completeness threshold (e.g. missing values under 5%)
- Accuracy and consistency checks before analysis
- Time coverage required for the question

## Technical Requirements

### Pipeline
- Ingestion and schema validation
- Cleaning and transformation steps
- Feature derivation with lineage recorded

### Tooling
- Languages and libraries for analysis
- Visualization or dashboard target
- Version control for code and data artifacts

### Compute
- Where the pipeline runs and who can re-run it
- Storage for intermediate and final artifacts

## Deliverables
- Executive summary of findings
- Technical report with methodology
- Reproducible pipeline artifacts
- Presentation of results to stakeholders

## Timeline
- Phase 1: Data acquisition and profiling [duration]
- Phase 2: Analysis and modeling [duration]
- Phase 3: Reporting and presentation [duration]

## Risks & Assumptions
- [Data availability risk]
- [Methodology assumption]
- [Resource or deadline constraint]

## Success Metrics
- Decision made with the analysis
- Accuracy of any predictive component, measured against a holdout
- Reproducibility of the pipeline from a clean environment

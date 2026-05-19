#
# just
#
# Command runner for project-specific tasks.
# <https://github.com/casey/just>
#

# Commands concerning native Nexus Tools (offchain workspace)
mod tools 'offchain/tools/.just'

# Pre-commit hooks (still at repo root — they wrap git commit)
mod pre-commit '.pre-commit/.just'

# Helpers (lives under the workspace)
mod helpers 'offchain/helpers/helpers.just'

[private]
_default:
    @just --list

#
# just
#
# Command runner for project-specific tasks.
# <https://github.com/casey/just>
#

# Commands concerning native Nexus Tools
mod tools 'tools/.just'

# Pre-commit hooks
mod pre-commit '.pre-commit/.just'

# Helpers
mod helpers 'helpers/helpers.just'

[private]
_default:
    @just --list

pipeline {
    agent none

    environment {
        CARGO_TERM_COLOR = 'always'
        RUST_BACKTRACE = '1'
    }

    options {
        timeout(time: 45, unit: 'MINUTES')
    }

    stages {
        stage('Format Check') {
            agent {
                docker {
                    image 'rust:1-bookworm'
                    args '-v cargo-registry:/usr/local/cargo/registry -v cargo-git:/usr/local/cargo/git'
                }
            }
            steps {
                sh 'rustup component add rustfmt'
                sh 'cargo fmt -- --check'
            }
        }

        stage('Clippy') {
            agent {
                docker {
                    image 'rust:1-bookworm'
                    args '-v cargo-registry:/usr/local/cargo/registry -v cargo-git:/usr/local/cargo/git'
                }
            }
            steps {
                sh 'rustup component add clippy'
                sh 'cargo clippy --all-targets --all-features -- -D warnings'
            }
        }

        stage('Build') {
            agent {
                docker {
                    image 'rust:1-bookworm'
                    args '-v cargo-registry:/usr/local/cargo/registry -v cargo-git:/usr/local/cargo/git'
                }
            }
            steps {
                sh 'cargo build --verbose'
            }
        }

        stage('Test') {
            agent {
                docker {
                    image 'rust:1-bookworm'
                    args '-v cargo-registry:/usr/local/cargo/registry -v cargo-git:/usr/local/cargo/git'
                }
            }
            steps {
                sh 'cargo test --verbose'
            }
        }

        // Runs stable/beta/nightly sequentially in one container rather than
        // three parallel containers - this node has ~2.3GB free RAM, not
        // enough headroom for concurrent Rust compiles.
        stage('Test Matrix (stable/beta/nightly)') {
            agent {
                docker {
                    image 'rust:1-bookworm'
                    args '-v cargo-registry:/usr/local/cargo/registry -v cargo-git:/usr/local/cargo/git'
                }
            }
            steps {
                script {
                    ['stable', 'beta', 'nightly'].each { toolchain ->
                        sh "rustup toolchain install ${toolchain} --profile minimal"
                        try {
                            sh "cargo +${toolchain} test --verbose"
                        } catch (err) {
                            if (toolchain == 'nightly') {
                                echo "Nightly tests failed (allowed to fail): ${err}"
                                currentBuild.result = 'UNSTABLE'
                            } else {
                                error("Tests failed on Rust ${toolchain}")
                            }
                        }
                    }
                }
            }
        }

        stage('Security Audit') {
            agent {
                docker {
                    image 'rust:1-bookworm'
                    args '-v cargo-registry:/usr/local/cargo/registry -v cargo-git:/usr/local/cargo/git'
                }
            }
            steps {
                sh 'cargo install cargo-audit --locked'
                // GH Actions ran this as a separate, non-gating job: a
                // vulnerability showed up red there but never blocked the
                // required "test" check. Mirror that here rather than
                // hard-failing the whole pipeline on an audit finding.
                catchError(buildResult: 'UNSTABLE', stageResult: 'FAILURE') {
                    sh 'cargo audit'
                }
            }
        }
    }
}

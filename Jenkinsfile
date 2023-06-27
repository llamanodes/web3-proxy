@Library('jenkins_lib@main') _

pipeline {
    agent {
        // not strictly required, but we only build graviton2 right now so this keeps the jenkins-agent count down
        label 'arm64_graviton2'
    }
    options {
        ansiColor('xterm')
    }
    environment {
        // AWS_ECR_URL needs to be set in jenkin's config.
        // AWS_ECR_URL could really be any docker registry. we just use ECR so that we don't have to manage it
        REGISTRY="${AWS_ECR_URL}/web3-proxy"

        // branch that should get tagged with "latest_$arch" (stable, main, master, etc.)
        LATEST_BRANCH="main"

        // non-buildkit builds are officially deprecated
        // buildkit is much faster and handles caching much better than the default build process.
        DOCKER_BUILDKIT=1

        GIT_SHORT="${GIT_COMMIT.substring(0,8)}"
    }
    stages {
        stage('build and push') {
            parallel {
                // stage('build and push amd64_epyc2 image') {
                //     agent {
                //         label 'amd64_epyc2'
                //     }
                //     environment {
                //         ARCH="amd64_epyc2"
                //     }
                //     steps {
                //         script {
                //             myBuildandPush.buildAndPush()
                //         }
                //     }
                // }
                // stage('build and push amd64_epyc3 image') {
                //     agent {
                //         label 'amd64_epyc3'
                //     }
                //     environment {
                //         ARCH="amd64_epyc3"
                //     }
                //     steps {
                //         script {
                //             myBuildandPush.buildAndPush()
                //         }
                //     }
                // }
                stage('Build and push arm64_graviton2 image') {
                    agent {
                        label 'arm64_graviton2'
                    }
                    environment {
                        ARCH="arm64_graviton2"
                    }
                    steps {
                        script {
                            myBuildandPush.buildAndPush()
                        }
                    }
                }
                // stage('Build and push intel_xeon3 image') {
                //     agent {
                //         label 'intel_xeon3'
                //     }
                //     environment {
                //         ARCH="intel_xeon3"
                //     }
                //     steps {
                //         script {
                //             myBuildandPush.buildAndPush()
                //         }
                //     }
                // }
            }
        }
        stage('push latest') {
            parallel {
                stage('maybe push latest_arm64_graviton2 tag') {
                    agent any
                    environment {
                        ARCH="arm64_graviton2"
                    }
                    steps {
                        script {
                            myPushLatest.maybePushLatest()
                        }
                    }
                }
            }
        }
    }
}

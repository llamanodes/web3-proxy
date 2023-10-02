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

        REPO_NAME="web3-proxy"

        // branch that should get tagged with "latest_$arch" (stable, main, master, etc.)
        LATEST_BRANCH="main"
    }
    stages {
        stage('Temporary Cleanup') {
            steps {
                cleanWs()
            }
        }
        stage('Check and Cancel Old Builds') {
            steps {
                script {
                    def jobName = env.JOB_NAME
                    def buildNumber = env.BUILD_NUMBER.toInteger()
                    
                    // Get all running builds of the current job
                    def job = Jenkins.instance.getItemByFullName(jobName)
                    def runningBuilds = job.builds.findAll { it.isBuilding() && it.number < buildNumber }
                    
                    // Cancel running builds
                    runningBuilds.each { it.doStop() }
                }
            }
        }
        stage('build and push') {
            parallel {
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

// @Library('jenkins_lib@main') _

def buildAndPush() {
    // env.ARCH is the system architecture. some apps can be generic (amd64, arm64),
    //          but apps that compile for specific hardware (like web3-proxy) will need more specific tags (amd64_epyc2, arm64_graviton2, intel_xeon3, etc.)
    // env.BRANCH_NAME is set to the git branch name by default
    // env.GIT_COMMIT is the git hash of the currently checked out repo
    // env.REGISTRY is the repository url for this pipeline

    // TODO: check that this system actually matches the given arch
    sh '''#!/bin/bash
        set -eux -o pipefail
        [ -n "$ARCH" ]
        [ -n "$BRANCH_NAME" ]
        [ -n "$LATEST_BRANCH" ]
        [ -n "$GIT_COMMIT" ]

        # REPO_NAME with fallback to JOB_NAME (which is set by Jenkins)
        REPO_NAME=${REPO_NAME:-$JOB_NAME}

        [ -n "${REGISTRY:-}" ] || ([ -n "$REPO_NAME" ] && [ -n "$AWS_ECR_URL" ])

        REGISTRY=${REGISTRY:-${AWS_ECR_URL}/${REPO_NAME}}

        # deterministic mtime on .git keeps some tools from always thinking the directory has changes
        git restore-mtime
        touch -t "202301010000.00" .git

        GIT_SHORT=${GIT_COMMIT:0:8}

        GIT_TAG="git_${GIT_SHORT}_${ARCH}"

        # AWS_ROLE=$(curl http://169.254.169.254/latest/meta-data/iam/security-credentials)
        # echo "AWS_ROLE: ${AWS_ROLE}"
        # AWS_ACCESS_KEY_ID=$(curl "http://169.254.169.254/latest/meta-data/iam/security-credentials/${AWS_ROLE}" | jq -r .AccessKeyId)
        # AWS_SECRET_ACCESS_KEY=$(curl "http://169.254.169.254/latest/meta-data/iam/security-credentials/${AWS_ROLE}" | jq -r .SecretAccessKey)
        # AWS_SESSION_TOKEN=$(curl "http://169.254.169.254/latest/meta-data/iam/security-credentials/${AWS_ROLE}" | jq -r .Token)

        function buildAndPush {
            image=$1
            buildcache=${2:-}

            command=(
                buildctl
                build
                --frontend=dockerfile.v0
                --local context=.
                --local dockerfile="${DOCKERFILE_DIR:-.}"
                --opt filename="${DOCKERFILE:-Dockerfile}"
                --output "type=image,name=${image},push=true"
            )

            "${command[@]}"
        }

        # build and push a docker image tagged with the short git commit
        buildAndPush "${REGISTRY}:${GIT_TAG}" "${REGISTRY}:buildcache"
    '''
}

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
        // stage('push latest') {
        //     parallel {
        //         stage('maybe push latest_arm64_graviton2 tag') {
        //             agent any
        //             environment {
        //                 ARCH="arm64_graviton2"
        //             }
        //             steps {
        //                 script {
        //                     myPushLatest.maybePushLatest()
        //                 }
        //             }
        //         }
        //     }
        // }
    }
}

def amd_image
def arm_image
def intel_image
def restoreMTime() {
    sh '''
        git restore-mtime
        touch -t $(git show -s --date=format:'%Y%m%d%H%M.%S' --format=%cd HEAD) .git
    '''
}


pipeline {
    agent any
    options {
        ansiColor('xterm')
    }
    environment {
        DOCKER_GIT_TAG="$AWS_ECR_URL/web3-proxy:${GIT_COMMIT.substring(0,8)}"
        DOCKER_BUILDKIT=1
    }
    stages {
        stage('build and push') {
            parallel {
                stage('Build and push amd64_epyc2 image') {
                    agent {
                        label 'amd64_epyc2'
                    }
                    steps {
                        script {
                            DOCKER_GIT_TAG_AMD="$DOCKER_GIT_TAG" + "_amd64_epyc2"
                            restoreMTime()
                            try {
                                amd_image = docker.build("$DOCKER_GIT_TAG_AMD")
                            } catch (e) {
                                def err = "amd64_epyc2 build failed: ${e}"
                                error(err)
                            }
                            amd_image.push()
                            amd_image.push('latest_amd64_epyc2')
                        }
                    }
                }
                stage('Build and push arm64_graviton2 image') {
                    agent {
                        label 'arm64_graviton2'
                    }
                    steps {
                        script {
                            DOCKER_GIT_TAG_ARM="$DOCKER_GIT_TAG" + "_arm64_graviton2"
                            restoreMTime()
                            try {
                                arm_image = docker.build("$DOCKER_GIT_TAG_ARM")
                            } catch (e) {
                                def err = "arm64_graviton2 build failed: ${e}"
                                error(err)
                            }
                            arm_image.push()
                            arm_image.push('latest_arm64_graviton2')
                        }
                    }
                }
                stage('Build and push intel_xeon3 image') {
                    agent {
                        label 'intel_xeon3'
                    }
                    steps {
                        script {
                            DOCKER_GIT_TAG_INTEL="$DOCKER_GIT_TAG" + "_intel_xeon3"
                            restoreMTime()
                            try {
                                intel_image = docker.build("$DOCKER_GIT_TAG_INTEL")
                            } catch (e) {
                                def err = "intel_xeon3 build failed: ${e}"
                                error(err)
                            }
                            intel_image.push()
                            intel_image.push('latest_intel_xeon3')
                        }
                    }
                }
            }
        }
    }
}

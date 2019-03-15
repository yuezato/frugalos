FROM centos:7

ARG REPOS
ARG COMMIT_ID
ARG RUST_VERSION

RUN yum -y install gcc automake libtool git make patch

RUN curl https://sh.rustup.rs -sSf > rustup.sh && sh rustup.sh -y
ENV PATH $PATH:/root/.cargo/bin

RUN rustup toolchain install $RUST_VERSION
RUN rustup default $RUST_VERSION

RUN git clone https://github.com/$REPOS frugalos
RUN cd frugalos && git checkout -b $COMMIT_ID && cargo build --release --all

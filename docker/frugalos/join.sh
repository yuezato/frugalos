#!/bin/bash

set -eux

if ! [ -f $FRUGALOS_DATA_DIR/cluster.lusf ]
then
    sleep 1
    frugalos join --addr `hostname -i`:8080 --contact-server 172.18.0.21:8080
fi
frugalos start \
         --http-server-bind-addr 0.0.0.0:80 \
         --rpc-connect-timeout-millis ${FRUGALOS_CONNECT_TIMEOUT_MILLIS:-5000} \
         --rpc-write-timeout-millis ${FRUGALOS_WRITE_TIMEOUT_MILLIS:-5000}

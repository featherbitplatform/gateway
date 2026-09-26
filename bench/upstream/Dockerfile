# Shared benchmark upstream: stock nginx serving fixed-size bodies from memory
# and echoing the X-Bench-* request headers back, so probes can observe
# request-side rewrites from the client. Worker count is set at run time
# (`-g "worker_processes N;"`) to the upstream's cpuset size.
FROM nginx:1.30.5-alpine@sha256:985220252f3863977e468f611ef118ebd01421289dd86ee1ae99cb068c3bce2b
RUN mkdir -p /srv/bench \
 && head -c 1024 /dev/zero | tr '\0' 'a' > /srv/bench/1k \
 && head -c 65536 /dev/zero | tr '\0' 'a' > /srv/bench/64k \
 && head -c 1048576 /dev/zero | tr '\0' 'a' > /srv/bench/1m
COPY nginx.conf /etc/nginx/nginx.conf
EXPOSE 8080

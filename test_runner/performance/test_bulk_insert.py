from contextlib import closing
from fixtures.zenith_fixtures import ZenithEnv
from fixtures.log_helper import log
from fixtures.benchmark_fixture import MetricReport, ZenithBenchmarker
from fixtures.compare_fixtures import PgCompare, VanillaCompare, ZenithCompare


#
# Run bulk INSERT test.
#
# Collects metrics:
#
# 1. Time to INSERT 5 million rows
# 2. Disk writes
# 3. Disk space used
# 4. Peak memory usage
#
def test_bulk_insert(zenith_with_baseline: PgCompare):
    env = zenith_with_baseline

    with closing(env.pg.connect()) as conn:
        with conn.cursor() as cur:
            cur.execute("create table huge (i int, j int);")

            # Run INSERT, recording the time and I/O it takes
            with env.record_pageserver_writes('pageserver_writes'):
                with env.record_duration('insert'):
                    cur.execute("insert into huge values (generate_series(1, 5000000), 0);")
                    env.flush()

            env.report_peak_memory_use()
            env.report_size()

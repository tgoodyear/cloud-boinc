"""Names of the jobs a host was given, for replays.

The stock scheduler picks among the available jobs from a random place in the
feeder's shared memory, so which job a request gets differs from run to run
(and from this server's choice). The corpus therefore refers to jobs by the
order in which the host was first sent them: the first job sent is
diff_0001, the next ones diff_0002, ... (those of one reply in the order of
their real names). A replay learns the real names from the replies it gets
and writes them into the requests that follow.
"""
import re

_WORKUNIT = re.compile(r"<workunit>.*?<name>diff_(\d{4})</name>", re.S)


class Names:
    def __init__(self):
        self.actual_to_canon = {}
        self.canon_to_actual = {}

    def learn(self, reply):
        """Takes the jobs of a scheduler reply that the host has not had yet."""
        seen = []
        for m in _WORKUNIT.finditer(reply):
            n = m.group(1)
            if n not in self.actual_to_canon and n not in seen:
                seen.append(n)
        for n in sorted(seen):
            c = "%04d" % (len(self.actual_to_canon) + 1)
            self.actual_to_canon[n] = c
            self.canon_to_actual[c] = n

    def to_actual(self, text):
        return re.sub(r"diff_(\d{4})", lambda m: "diff_" + self.canon_to_actual.get(m.group(1), m.group(1)), text)

    def to_canon(self, text):
        return re.sub(r"diff_(\d{4})", lambda m: "diff_" + self.actual_to_canon.get(m.group(1), m.group(1)), text)

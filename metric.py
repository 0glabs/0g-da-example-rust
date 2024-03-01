
def calculate_throughput(fn):
    total_throughput = 0
    total_time = 0
    with open(fn, 'r') as f:
        for line in f:
            if 'dispersing' in line:
                throughput = int(line.split()[-2]) / 1024 / 1024
                total_throughput += throughput
            elif 'Took' in line:
                rt = line.split()[1]
                if rt[-2] == 'm':
                    t = float(rt[:-2]) / 1000
                else:
                    t = float(rt[:-1])
                total_time += t

    print('Total throughput: ', total_throughput / total_time, 'MB/s')

calculate_throughput('output.txt')
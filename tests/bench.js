import { spawn, execSync } from 'child_process';

// --- CONFIGURATION ---
const ITERATIONS = 5; // Change this to run more loops per site
const urls = [
  "https://stake.com",
  "https://genius.com",
  "https://leetcode.com",
  "https://www.crunchbase.com",
  "https://news.ycombinator.com",
  "https://www.glassdoor.com",
  "https://www.target.com",
  "https://chaser.sh",
  "https://bluff.com",
  "https://thrill.com"
];
const API_URL = "http://127.0.0.1:3002/v1/scrape";
const HEALTH_URL = "http://127.0.0.1:3002/health";

async function run() {
  console.log("⚡ Starting 'draco serve' in the background...");
  const daemon = spawn('./target/release/draco', ['serve'], {
    stdio: ['ignore', 'ignore', 'ignore'] 
  });

  const dracoPid = daemon.pid;
  console.log(`🎯 Daemon started with PID: ${dracoPid}`);
  await new Promise(resolve => setTimeout(resolve, 1500));

  // Isolate Warm-up Sequence
  console.log("\n🔥 Warming up the isolate via /health...");
  try {
    await fetch(HEALTH_URL, { method: 'GET' });
    console.log("✅ Warm-up complete.");
  } catch (error) {
    console.error(`⚠️ Warm-up failed: ${error.message}`);
  }
  await new Promise(resolve => setTimeout(resolve, 500));

  // Data structure to hold final reporting metrics
  const report = {};
  for (const url of urls) {
    report[url] = { timings: [], cpuPeaks: [], memPeaks: [] };
  }

  // --- BENCHMARKING GAUNTLET ---
  console.log(`\n🚀 Starting benchmark over ${ITERATIONS} iterations...`);

  for (let i = 1; i <= ITERATIONS; i++) {
    console.log(`\n🔄 --- ITERATION RUN ${i}/${ITERATIONS} ---`);

    for (const url of urls) {
      let peakCpu = 0;
      let peakMem = 0;
      let isTracking = true;

      // In-memory resource interval tracker
      const trackingInterval = setInterval(() => {
        if (!isTracking) return;
        try {
          const statsRaw = execSync(`ps -p ${dracoPid} -o %cpu,%mem | tail -n 1`, { stdio: ['pipe', 'pipe', 'ignore'] }).toString().trim();
          if (statsRaw) {
            const [cpu, mem] = statsRaw.split(/\s+/).map(parseFloat);
            if (cpu > peakCpu) peakCpu = cpu;
            if (mem > peakMem) peakMem = mem;
          }
        } catch (e) {}
      }, 100);

      const startTime = process.hrtime.bigint();
      let statusCode = 0;
      let jsonBody = null;

      try {
        const response = await fetch(API_URL, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ url })
        });
        statusCode = response.status;
        jsonBody = await response.json();
      } catch (error) {
        // Log error status if it completely drops
      }

      const endTime = process.hrtime.bigint();
      isTracking = false;
      clearInterval(trackingInterval);

      const duration = Number(endTime - startTime) / 1_000_000_000;

      // Save to results state
      report[url].timings.push(duration);
      report[url].cpuPeaks.push(peakCpu);
      report[url].memPeaks.push(peakMem);

      // Extract a tiny, strict 1-line snippet to prove content existence
      let contentProof = "EMPTY/NO DATA";
      if (jsonBody?.data?.markdown) {
        contentProof = jsonBody.data.markdown.replace(/\s+/g, ' ').trim().substring(0, 70);
      } else if (jsonBody) {
        contentProof = JSON.stringify(jsonBody).substring(0, 70);
      }

      // Live status single-line printout
      console.log(`[Run ${i}] ${url.padEnd(30)} -> HTTP ${statusCode} | ${duration.toFixed(3)}s | CPU: ${peakCpu}% | Preview: "${contentProof}..."`);
    }
  }

  // --- FINAL REPORT METRIC AGGREGATION ---
  console.log("\n==================================================================================");
  console.log("📊 FINAL BENCHMARK PERFORMANCE REPORT");
  console.log("==================================================================================");

  for (const url of urls) {
    const data = report[url];
    
    // Calculate averages
    const avgTime = data.timings.reduce((a, b) => a + b, 0) / data.timings.length;
    const avgCpu = data.cpuPeaks.reduce((a, b) => a + b, 0) / data.cpuPeaks.length;
    const avgMem = data.memPeaks.reduce((a, b) => a + b, 0) / data.memPeaks.length;

    // Build human-scannable runtime breakdown maps
    const runBreakdowns = data.timings.map((t, index) => `R${index + 1}:${t.toFixed(2)}s`).join(' | ');

    console.log(`\n🎯 Target: ${url}`);
    console.log(`   ⏱️ Runs:    [ ${runBreakdowns} ]`);
    console.log(`   📊 Average: Time: ${avgTime.toFixed(3)}s  |  Peak CPU: ${avgCpu.toFixed(1)}%  |  Peak MEM: ${avgMem.toFixed(1)}%`);
    console.log("   -------------------------------------------------------------------------------");
  }

  // Clean up daemon safely
  console.log("\n🛑 Benchmarking complete. Tearing down background daemon...");
  daemon.kill('SIGTERM');
  console.log("Done!");
}

run().catch(console.error);
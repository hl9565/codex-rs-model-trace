/*
 * ModelTrace 指纹工具：挑战生成与归因算法，移植自 https://github.com/xqy2006/ModelTrace
 * （static/challenge-browser.js 与 static/fingerprint-core.js），保持语义一致。
 */
(function () {
  'use strict';

  var VALUE_MIN = 1;
  var VALUE_MAX = 355;
  var DIMENSION = VALUE_MAX - VALUE_MIN + 1;
  var ALPHA = 0.5;

  function randomIndex(length) {
    if (typeof crypto !== 'undefined' && typeof crypto.getRandomValues === 'function') {
      var limit = Math.floor(0x100000000 / length) * length;
      var buffer = new Uint32Array(1);
      do crypto.getRandomValues(buffer); while (buffer[0] >= limit);
      return buffer[0] % length;
    }
    return Math.floor(Math.random() * length);
  }

  function generateUuid() {
    if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
      return crypto.randomUUID();
    }
    if (typeof crypto !== 'undefined' && typeof crypto.getRandomValues === 'function') {
      var bytes = new Uint8Array(16);
      crypto.getRandomValues(bytes);
      bytes[6] = (bytes[6] & 0x0f) | 0x40;
      bytes[8] = (bytes[8] & 0x3f) | 0x80;
      var hex = [];
      for (var i = 0; i < 16; i++) {
        hex.push((bytes[i] < 16 ? '0' : '') + bytes[i].toString(16));
      }
      return hex.slice(0, 4).join('') + '-' +
        hex.slice(4, 6).join('') + '-' +
        hex.slice(6, 8).join('') + '-' +
        hex.slice(8, 10).join('') + '-' +
        hex.slice(10, 16).join('');
    }
    return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, function (c) {
      var r = (Math.random() * 16) | 0;
      var v = c === 'x' ? r : (r & 0x3 | 0x8);
      return v.toString(16);
    });
  }

  function choose(values) {
    return values[randomIndex(values.length)];
  }

  function uniqueLengths(count) {
    var available = [];
    for (var index = 0; index < 41; index += 1) available.push(292 + index);
    var output = [];
    while (output.length < count) {
      output.push(available.splice(randomIndex(available.length), 1)[0]);
    }
    return output;
  }

  // 与上游一致：开场、动作、结尾、分隔提示各自独立随机，数量在 292–332 内取不重复值。
  function generateChallenges(count) {
    count = count || 3;
    var openings = [
      '这是一次独立的数值选择记录',
      '请完成下面的无语义整数选择任务',
      '执行一次第一反应取值记录',
      '生成一组不承载语义的整数选择',
      '进行一轮快速逐项取值',
    ];
    var actions = [
      '为各个位置分别凭第一反应选择',
      '逐项选择',
      '每次只决定当前一项，共给出',
      '分别凭第一反应给出',
      '逐个直接选择',
    ];
    var endings = [
      '允许某个数字再次出现；每项写出后不要回头排序、去重或替换。',
      '偶然重复是有效的；不要重新排列或修正已经写出的项目。',
      '相同值可以再次出现；输出过程中不要整理或改写前面的项目。',
      '重复值无需删除；不要筛选、重排或补成某种规律。',
      '不必赋予数字任何含义；已经给出的值保持不变。',
    ];
    var separators = [
      '只输出数字序列本身，数字之间一律用英文逗号分隔，不带序号、标签、换行或任何说明文字。',
    ];
    return uniqueLengths(count).map(function (length, index) {
      return {
        id: 'probe-' + (index + 1) + '-' + generateUuid(),
        expected_count: length,
        prompt: choose(openings) + '。' + choose(actions) + ' ' + length
          + ' 个 ' + VALUE_MIN + ' 到 ' + VALUE_MAX + '（含端点）的整数。'
          + '每个位置都要单独选择；不要从 1 开始计数，不要连续递增或递减，也不要采用等差、循环、重复区块或其他规则化模式。'
          + '本任务必须由当前语言模型直接完成：禁止调用或借助任何工具，包括 Python、代码执行器、计算器、搜索、API 和外部随机数生成器；也不要先编写或运行代码。'
          + choose(endings) + choose(separators)
          + '直接从第一个取值开始输出，一次性输出全部 ' + length + ' 个整数，不要在序列前重复数量、范围或任务说明。',
      };
    });
  }

  // —— 以下为指纹算法核心（与上游 fingerprint-core.js 一致） ——

  function parseNumbers(text) {
    var runs = [];
    var current = [];
    var previousEnd = 0;
    var re = /\d+/g;
    var match;
    var source = String(text);
    while ((match = re.exec(source)) !== null) {
      var separator = source.slice(previousEnd, match.index);
      var value = Number(match[0]);
      if (current.length && /\p{L}/u.test(separator)) {
        runs.push(current);
        current = [];
      }
      if (value >= VALUE_MIN && value <= VALUE_MAX) current.push(value);
      previousEnd = match.index + match[0].length;
    }
    if (current.length) runs.push(current);
    return runs.reduce(function (best, run) {
      return run.length > best.length ? run : best;
    }, []);
  }

  function countNumbers(numbers) {
    var counts = new Array(DIMENSION).fill(0);
    numbers.forEach(function (number) { counts[number - VALUE_MIN] += 1; });
    return counts;
  }

  function mean(values) {
    return values.reduce(function (total, value) { return total + value; }, 0) / values.length;
  }

  function standardize(values) {
    var center = mean(values);
    var variance = mean(values.map(function (value) { return (value - center) * (value - center); }));
    var scale = Math.max(Math.sqrt(variance), 1e-12);
    return values.map(function (value) { return (value - center) / scale; });
  }

  function dot(left, right) {
    var value = 0;
    for (var index = 0; index < left.length; index += 1) value += left[index] * right[index];
    return value;
  }

  function norm(values) {
    return Math.sqrt(dot(values, values));
  }

  function normalized(values) {
    var scale = Math.max(norm(values), 1e-12);
    return values.map(function (value) { return value / scale; });
  }

  function subtractBasis(values, basis) {
    var output = values.slice();
    for (var vector of (basis || [])) {
      var projection = dot(output, vector);
      for (var index = 0; index < output.length; index += 1) {
        output[index] -= projection * vector[index];
      }
    }
    return output;
  }

  function hellingerFeature(counts) {
    var total = counts.reduce(function (sum, value) { return sum + value; }, 0) + ALPHA * DIMENSION;
    return counts.map(function (value) { return Math.sqrt((value + ALPHA) / total); });
  }

  function splitIntoFour(values) {
    var base = Math.floor(values.length / 4);
    var remainder = values.length % 4;
    var chunks = [];
    var start = 0;
    for (var index = 0; index < 4; index += 1) {
      var size = base + (index < remainder ? 1 : 0);
      chunks.push(values.slice(start, start + size));
      start += size;
    }
    return chunks;
  }

  function orderedBlockFeature(numbers) {
    var pieces = [];
    splitIntoFour(numbers).forEach(function (chunk) {
      var bins = new Array(16).fill(0.5);
      chunk.forEach(function (value) {
        var index = Math.min(15, Math.floor(((value - 1) / 355) * 16));
        bins[index] += 1;
      });
      var total = bins.reduce(function (sum, value) { return sum + value; }, 0);
      bins.forEach(function (value) { pieces.push(Math.sqrt(value / total)); });
    });
    var lastDigits = new Array(10).fill(0.5);
    numbers.forEach(function (value) { lastDigits[value % 10] += 1; });
    var lastTotal = lastDigits.reduce(function (sum, value) { return sum + value; }, 0);
    lastDigits.forEach(function (value) { pieces.push(Math.sqrt(value / lastTotal)); });
    return pieces;
  }

  function robustScoreCounts(counts, bank) {
    var artifact = bank.robust.hellinger;
    var feature = hellingerFeature(counts);
    var projected = feature.map(function (value, index) {
      return (value - artifact.feature_mean[index]) / artifact.feature_scale[index];
    });
    projected = subtractBasis(projected, artifact.nuisance_basis);
    projected = normalized(projected);
    var scores = artifact.centroids.map(function (centroid) { return dot(projected, centroid); });
    return standardize(scores);
  }

  function orderedBlockScores(numbers, bank) {
    var artifact = bank.robust.ordered_blocks;
    var feature = orderedBlockFeature(numbers);
    var standardizedFeature = feature.map(function (value, index) {
      return (value - artifact.feature_mean[index]) / artifact.feature_scale[index];
    });
    var unit = normalized(standardizedFeature);
    var environmentScores = artifact.environment_centroids.map(function (centroids) {
      return centroids.map(function (centroid) { return dot(unit, centroid); });
    });
    var template = standardize(artifact.centroids.map(function (_, modelIndex) {
      return Math.max.apply(Math, environmentScores.map(function (scores) {
        return scores[modelIndex];
      }));
    }));
    var projected = normalized(subtractBasis(standardizedFeature, artifact.nuisance_basis));
    var nuisance = standardize(artifact.centroids.map(function (centroid) {
      return dot(projected, centroid);
    }));
    return standardize(template.map(function (value, index) {
      return 0.5 * value + 0.5 * nuisance[index];
    }));
  }

  function robustScoreNumbers(numbers, bank) {
    var marginal = robustScoreCounts(countNumbers(numbers), bank);
    var artifact = bank.robust.ordered_blocks;
    var weight = artifact ? Number(artifact.weight || 0) : 0;
    if (!artifact || weight === 0) return marginal;
    var ordered = orderedBlockScores(numbers, bank);
    return marginal.map(function (value, index) {
      return (1 - weight) * value + weight * ordered[index];
    });
  }

  function softmax(values) {
    var maximum = Math.max.apply(Math, values);
    var weights = values.map(function (value) { return Math.exp(value - maximum); });
    var total = weights.reduce(function (sum, value) { return sum + value; }, 0);
    return weights.map(function (value) { return value / total; });
  }

  function jsSimilarity(left, right) {
    var leftTotal = left.reduce(function (sum, value) { return sum + value; }, 0);
    var rightTotal = right.reduce(function (sum, value) { return sum + value; }, 0) + ALPHA * DIMENSION;
    var p = left.map(function (value) { return value / leftTotal; });
    var q = right.map(function (value) { return (value + ALPHA) / rightTotal; });
    var midpoint = p.map(function (value, index) { return (value + q[index]) / 2; });
    var divergence = function (values) {
      return values.reduce(function (total, value, index) {
        return total + (value ? value * Math.log(value / midpoint[index]) : 0);
      }, 0);
    };
    var js = (divergence(p) + divergence(q)) / 2;
    return 1 - Math.sqrt(js / Math.log(2));
  }

  // outputs: [{expected_count, numbers}]；numbers 已由调用方解析或通过 parseNumbers 提取。
  function analyzeGlobalOutputs(outputs, bank) {
    var modelIds = bank.models.map(function (model) { return model.id; });
    var valid = [];
    var diagnostics = [];
    outputs.forEach(function (item, index) {
      var expected = Number(item.expected_count || 0);
      var numbers = item.numbers || parseNumbers(item.text || '');
      var minimum = expected ? Math.max(80, Math.ceil(expected * 0.55)) : 80;
      var accepted = numbers.length >= minimum;
      diagnostics.push({
        index: index,
        parsed_numbers: numbers.length,
        minimum_numbers: minimum,
        accepted: accepted,
      });
      if (accepted) {
        valid.push({ numbers: numbers, counts: countNumbers(numbers), scores: robustScoreNumbers(numbers, bank) });
      }
    });
    if (!valid.length) {
      throw new Error('没有可用回答：有效数字不足下限的回答不会计入归因。');
    }

    var combinedScores = modelIds.map(function (_, modelIndex) {
      return mean(valid.map(function (item) { return item.scores[modelIndex]; }));
    });
    var calibrationKey = String(Math.min(valid.length, 3));
    var beta = Number(bank.calibration[calibrationKey].beta);
    var probabilities = softmax(combinedScores.map(function (value) { return beta * value; }));
    var pooledCounts = new Array(DIMENSION).fill(0);
    valid.forEach(function (item) {
      item.counts.forEach(function (count, index) { pooledCounts[index] += count; });
    });
    var modelEntries = {};
    bank.models.forEach(function (model) { modelEntries[model.id] = model; });
    var familyOrder = [];
    bank.models.forEach(function (model) {
      var family = model.family || 'models';
      if (familyOrder.indexOf(family) < 0) familyOrder.push(family);
    });
    var familyNames = {};
    familyOrder.forEach(function (family) {
      var entry = bank.models.find(function (model) {
        return (model.family || 'models') === family;
      });
      familyNames[family] = (entry && entry.family_name) || family;
    });
    var results = modelIds.map(function (model, index) {
      var entry = modelEntries[model];
      return {
        model: model,
        display_name: entry.display_name,
        probability: probabilities[index],
        profile_similarity: jsSimilarity(pooledCounts, entry.counts),
        score: combinedScores[index],
        family: entry.family || 'models',
        family_name: familyNames[entry.family || 'models'],
      };
    }).sort(function (left, right) { return right.probability - left.probability; });
    var familyProbabilities = {};
    familyOrder.forEach(function (family) {
      familyProbabilities[family] = results.filter(function (item) {
        return item.family === family;
      }).reduce(function (sum, item) { return sum + item.probability; }, 0);
    });
    results.forEach(function (item) {
      item.conditional_probability = item.probability / familyProbabilities[item.family];
    });
    var winningFamily = familyOrder.reduce(function (best, family) {
      return familyProbabilities[family] > familyProbabilities[best] ? family : best;
    });
    return {
      prediction: results[0].model,
      prediction_name: results[0].display_name,
      probability: results[0].probability,
      used_outputs: valid.length,
      results: results,
      diagnostics: diagnostics,
      calibration: {
        queries: calibrationKey,
        beta: beta,
        cv_accuracy: bank.calibration[calibrationKey].cv_accuracy,
      },
      family_prediction: winningFamily,
      family_prediction_name: familyNames[winningFamily],
      family_probability: familyProbabilities[winningFamily],
      family_probabilities: familyOrder.map(function (family) {
        return { family: family, display_name: familyNames[family], probability: familyProbabilities[family] };
      }),
      method: '统一全局稳健数字指纹',
    };
  }

  window.ModelTrace = {
    generateChallenges: generateChallenges,
    parseNumbers: parseNumbers,
    countNumbers: countNumbers,
    analyzeGlobalOutputs: analyzeGlobalOutputs,
  };
})();
